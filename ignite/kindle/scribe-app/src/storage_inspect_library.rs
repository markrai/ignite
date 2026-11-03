//! Bounded, strictly read-only inspection of one native Kindle notebook library.
//!
//! This module deliberately does not call any of the normal open/recovery entry points. Those
//! entry points may create a library, migrate page headers, truncate torn journals, repair
//! backgrounds, allocate sync identities, or finish interrupted transactions. Inspection opens
//! existing regular files read-only, refuses links/special files, and reports required work.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use kindle_platform::rect_checksum;
use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sync_protocol::IdentityFileV1;

use crate::folders::{FoldersFileV1, FOLDERS_DIR, FOLDERS_FILE};
use crate::library::{
    validate_library_manifest, validate_title, PersistedLibraryManifestV1, PersistedNotebookInfoV1,
    LIBRARY_MANIFEST_FILE, NOTEBOOKS_DIR, NOTEBOOK_INFO_FILE, NOTEBOOK_TRASH_DIR,
};
use crate::nbk_import::{
    receipt_key, ImportOriginV1, ReceiptState, ReceiptsV1, IMPORT_DIR, ORIGIN_KIND, ORIGIN_MARKER,
    RECEIPTS_FILE,
};
use crate::notebook::{
    validate_manifest as validate_notebook_manifest, PersistedNotebookManifestV1, MANIFEST_FILE,
    PAGE_TRASH_DIR,
};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::profile::{
    validate_persisted_render_profile_v1, validate_persisted_render_profile_v2,
    PersistedRenderProfileV1, PersistedRenderProfileV2, RenderProfile, RENDER_PROFILE_VERSION,
    RENDER_PROFILE_VERSION_V1,
};
use crate::store::{
    parse_edit_record_line, parse_stroke_record, validate_persisted_canonical_xy,
    validate_replace_strokes_structural, BackgroundIdentityV1, EditOp, PersistedPageHeader,
    PersistedReplacementV2, BG_LAYOUT_VERSION, EDIT_JOURNAL_VERSION, EDIT_JOURNAL_VERSION_V1,
    FORMAT_VERSION, FORMAT_VERSION_V2, FORMAT_VERSION_V3, FORMAT_VERSION_V4,
};
use crate::sync_local::{IDENTITY_FILE, IDENTITY_LOCK_FILE, SYNC_DIR};

pub const REPORT_SCHEMA: u32 = 1;
pub const REPORT_MAX_BYTES: usize = 512 * 1024;
pub const MAX_FINDINGS: usize = 512;
pub const MAX_TREE_ENTRIES: usize = 200_000;
pub const MAX_NOTEBOOKS: usize = 10_000;
pub const MAX_PAGES: usize = 100_000;
pub const MAX_RECORDS: u64 = 2_000_000;
pub const MAX_TREE_DEPTH: usize = 8;
pub const MAX_RELATIVE_PATH_BYTES: usize = 512;
pub const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_PAGE_META_BYTES: u64 = 256 * 1024;
pub const MAX_BACKGROUND_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_JOURNAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_TOTAL_BYTES_PER_PASS: u64 = 16 * 1024 * 1024 * 1024;
pub const MAX_JSON_DEPTH: usize = 64;
pub const MAX_JSON_NODES: usize = 1_000_000;
pub const MAX_POINTS_PER_STROKE: usize = 100_000;

const MIGRATION_RECORD_FILE: &str = ".scribe-library-migration-v1";
const MIGRATION_STAGE_NAME: &str = ".notebook-1.migrating";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticClass {
    Valid,
    UnsupportedFormat,
    Corrupt,
    IncompleteTransaction,
    MissingRequired,
    Unverified,
    InspectionError,
}

impl DiagnosticClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "VALID",
            Self::UnsupportedFormat => "UNSUPPORTED_FORMAT",
            Self::Corrupt => "CORRUPT",
            Self::IncompleteTransaction => "INCOMPLETE_TRANSACTION",
            Self::MissingRequired => "MISSING_REQUIRED",
            Self::Unverified => "UNVERIFIED",
            Self::InspectionError => "INSPECTION_ERROR",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Valid => 0,
            Self::UnsupportedFormat => 1,
            Self::Corrupt => 2,
            Self::IncompleteTransaction => 3,
            Self::MissingRequired => 4,
            Self::Unverified => 5,
            Self::InspectionError => 6,
        }
    }
}

const ALL_CLASSES: [DiagnosticClass; 7] = [
    DiagnosticClass::Valid,
    DiagnosticClass::UnsupportedFormat,
    DiagnosticClass::Corrupt,
    DiagnosticClass::IncompleteTransaction,
    DiagnosticClass::MissingRequired,
    DiagnosticClass::Unverified,
    DiagnosticClass::InspectionError,
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub class: DiagnosticClass,
    pub relative_path: String,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InspectionCounts {
    pub libraries: u64,
    pub notebooks: u64,
    pub pages: u64,
    pub stroke_records: u64,
    pub edit_records: u64,
    pub text_records: u64,
    pub link_records: u64,
    pub folder_records: u64,
    pub import_receipts: u64,
    pub bytes_examined: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryInspectionReport {
    pub completed: bool,
    pub counts: InspectionCounts,
    pub observed_page_formats: BTreeSet<u32>,
    diagnostic_counts: [u64; 7],
    findings: Vec<Finding>,
    findings_overflowed: bool,
}

impl Default for LibraryInspectionReport {
    fn default() -> Self {
        Self {
            completed: true,
            counts: InspectionCounts::default(),
            observed_page_formats: BTreeSet::new(),
            diagnostic_counts: [0; 7],
            findings: Vec::new(),
            findings_overflowed: false,
        }
    }
}

impl LibraryInspectionReport {
    pub fn diagnostic_count(&self, class: DiagnosticClass) -> u64 {
        self.diagnostic_counts[class.index()]
    }

    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    pub fn result_token(&self) -> &'static str {
        if !self.completed {
            "INSPECTION_INCOMPLETE"
        } else if ALL_CLASSES[1..]
            .iter()
            .any(|class| self.diagnostic_count(*class) != 0)
        {
            "ISSUES_DETECTED"
        } else {
            "VALID"
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self.result_token() {
            "VALID" => 0,
            "ISSUES_DETECTED" => 2,
            _ => 1,
        }
    }

    fn observe(&mut self, class: DiagnosticClass) {
        self.diagnostic_counts[class.index()] =
            self.diagnostic_counts[class.index()].saturating_add(1);
        if class == DiagnosticClass::InspectionError {
            self.completed = false;
        }
    }

    fn finding(&mut self, class: DiagnosticClass, path: impl Into<String>, reason: &'static str) {
        self.observe(class);
        if self.findings.len() < MAX_FINDINGS {
            self.findings.push(Finding {
                class,
                relative_path: path.into(),
                reason,
            });
        } else if !self.findings_overflowed {
            self.findings_overflowed = true;
            self.completed = false;
            self.diagnostic_counts[DiagnosticClass::InspectionError.index()] =
                self.diagnostic_counts[DiagnosticClass::InspectionError.index()].saturating_add(1);
        }
    }

    fn finding_once(
        &mut self,
        class: DiagnosticClass,
        path: impl Into<String>,
        reason: &'static str,
    ) {
        let path = path.into();
        if !self.findings.iter().any(|finding| {
            finding.class == class && finding.relative_path == path && finding.reason == reason
        }) {
            self.finding(class, path, reason);
        }
    }

    fn finish(&mut self) {
        if self.findings_overflowed {
            self.findings.push(Finding {
                class: DiagnosticClass::InspectionError,
                relative_path: ".".into(),
                reason: "finding_limit_exceeded",
            });
        }
        self.findings.sort();
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, String> {
        let mut out = String::new();
        out.push_str("IGNITE_STORAGE_LIBRARY_INSPECTION_V1\n");
        out.push_str("schema=1\n");
        out.push_str("scope=library-only\n");
        out.push_str("coverage=partial\n");
        out.push_str("writer_exclusion=false\n");
        out.push_str("atomic_snapshot=false\n");
        out.push_str("external_sync=out-of-scope\n");
        out.push_str("device_state=out-of-scope\n");
        out.push_str("data_compatibility=unknown\n");
        out.push_str("promotion_authorized=false\n");
        out.push_str("inspection_action=report-only\n");
        out.push_str(&format!("completed={}\n", self.completed));
        out.push_str(&format!("result={}\n", self.result_token()));
        out.push_str(&format!("libraries={}\n", self.counts.libraries));
        out.push_str(&format!("notebooks={}\n", self.counts.notebooks));
        out.push_str(&format!("pages={}\n", self.counts.pages));
        out.push_str(&format!("stroke_records={}\n", self.counts.stroke_records));
        out.push_str(&format!("edit_records={}\n", self.counts.edit_records));
        out.push_str(&format!("text_records={}\n", self.counts.text_records));
        out.push_str(&format!("link_records={}\n", self.counts.link_records));
        out.push_str(&format!("folder_records={}\n", self.counts.folder_records));
        out.push_str(&format!(
            "import_receipts={}\n",
            self.counts.import_receipts
        ));
        out.push_str(&format!("bytes_examined={}\n", self.counts.bytes_examined));
        let formats = if self.observed_page_formats.is_empty() {
            "none".to_string()
        } else {
            self.observed_page_formats
                .iter()
                .map(|v| format!("v{v}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        out.push_str(&format!("observed_page_formats={formats}\n"));
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
                return Err("unsafe inspection report field".into());
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

pub(crate) fn safe_report_field(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RELATIVE_PATH_BYTES
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'_' | b'-' | b'=' | b':')
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectionCommand {
    NotRequested,
    Inspect(PathBuf),
}

pub fn parse_command(args: &[String]) -> Result<InspectionCommand, String> {
    let count = args
        .iter()
        .filter(|arg| arg.as_str() == "--inspect-storage-library")
        .count();
    if count == 0 {
        return Ok(InspectionCommand::NotRequested);
    }
    if count != 1
        || args.len() != 3
        || args[0] != "--inspect-storage-library"
        || args[1] != "--data-dir"
        || args[2].is_empty()
    {
        return Err(
            "--inspect-storage-library must be used exactly as: --inspect-storage-library --data-dir PATH"
                .into(),
        );
    }
    Ok(InspectionCommand::Inspect(PathBuf::from(&args[2])))
}

#[derive(Debug)]
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value without duplicate object keys")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(|n| StrictValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictValue(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::String(value)))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<StrictValue>()? {
                    values.push(value.0);
                }
                Ok(StrictValue(Value::Array(values)))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                let mut seen = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate-key"));
                    }
                    let value = map.next_value::<StrictValue>()?;
                    values.insert(key, value.0);
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonFailure {
    DuplicateKey,
    Invalid,
    TooDeep,
    TooManyNodes,
}

pub(crate) fn parse_json_strict(bytes: &[u8]) -> Result<Value, JsonFailure> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictValue::deserialize(&mut deserializer).map_err(|error| {
        if error.to_string().contains("duplicate-key") {
            JsonFailure::DuplicateKey
        } else {
            JsonFailure::Invalid
        }
    })?;
    deserializer.end().map_err(|_| JsonFailure::Invalid)?;
    let mut nodes = 0usize;
    validate_json_shape(&value.0, 0, &mut nodes)?;
    Ok(value.0)
}

fn validate_json_shape(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), JsonFailure> {
    if depth > MAX_JSON_DEPTH {
        return Err(JsonFailure::TooDeep);
    }
    *nodes = nodes.saturating_add(1);
    if *nodes > MAX_JSON_NODES {
        return Err(JsonFailure::TooManyNodes);
    }
    match value {
        Value::Array(values) => {
            for value in values {
                validate_json_shape(value, depth + 1, nodes)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_json_shape(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn json_reason(error: JsonFailure) -> &'static str {
    match error {
        JsonFailure::DuplicateKey => "duplicate_json_key",
        JsonFailure::Invalid => "invalid_json",
        JsonFailure::TooDeep => "json_nesting_limit_exceeded",
        JsonFailure::TooManyNodes => "json_node_limit_exceeded",
    }
}

fn object_has_unknown_fields(value: &Value, allowed: &[&str]) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object
        .keys()
        .any(|key| !allowed.iter().any(|allowed| key == allowed))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CensusEntry {
    kind: &'static str,
    size: u64,
    sha256: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Census {
    entries: BTreeMap<String, CensusEntry>,
    complete: bool,
}

pub(crate) fn inspect_metadata(path: &Path) -> Result<fs::Metadata, &'static str> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "metadata_unavailable")?;
    if metadata.file_type().is_symlink() {
        return Err("symlink_refused");
    }
    Ok(metadata)
}

#[cfg(unix)]
pub(crate) fn open_regular_read_only(
    path: &Path,
    expected: &fs::Metadata,
) -> Result<File, &'static str> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    if expected.nlink() != 1 {
        return Err("hard_link_refused");
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "read_open_failed")?;
    let actual = file.metadata().map_err(|_| "metadata_unavailable")?;
    if !actual.is_file()
        || actual.dev() != expected.dev()
        || actual.ino() != expected.ino()
        || actual.nlink() != 1
    {
        return Err("file_changed_during_open");
    }
    Ok(file)
}

#[cfg(not(unix))]
pub(crate) fn open_regular_read_only(
    path: &Path,
    expected: &fs::Metadata,
) -> Result<File, &'static str> {
    if !expected.is_file() {
        return Err("unsupported_file_type");
    }
    File::open(path).map_err(|_| "read_open_failed")
}

pub(crate) fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, &'static str> {
    let metadata = inspect_metadata(path)?;
    if !metadata.is_file() {
        return Err("unsupported_file_type");
    }
    if metadata.len() > max_bytes {
        return Err("file_size_limit_exceeded");
    }
    let file = open_regular_read_only(path, &metadata)?;
    let capacity = usize::try_from(metadata.len()).map_err(|_| "file_size_limit_exceeded")?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| "read_failed")?;
    if bytes.len() as u64 != metadata.len() {
        return Err("file_changed_during_read");
    }
    Ok(bytes)
}

pub(crate) fn safe_component(name: &std::ffi::OsStr) -> Option<String> {
    let value = name.to_str()?;
    if value.is_empty()
        || value.len() > 255
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return None;
    }
    Some(value.to_owned())
}

pub(crate) fn join_rel(parent: &str, child: &str) -> Option<String> {
    let joined = if parent == "." {
        child.to_owned()
    } else {
        format!("{parent}/{child}")
    };
    (joined.len() <= MAX_RELATIVE_PATH_BYTES).then_some(joined)
}

pub(crate) fn hash_file(path: &Path, metadata: &fs::Metadata) -> Result<String, &'static str> {
    let mut file = open_regular_read_only(path, metadata)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        let count = file.read(&mut buffer).map_err(|_| "read_failed")?;
        if count == 0 {
            break;
        }
        read = read.saturating_add(count as u64);
        hasher.update(&buffer[..count]);
    }
    if read != metadata.len() {
        return Err("file_changed_during_read");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_census(root: &Path, report: &mut LibraryInspectionReport) -> Census {
    let mut census = Census {
        entries: BTreeMap::new(),
        complete: true,
    };
    if path_chain_contains_symlink(root) {
        report.finding(
            DiagnosticClass::InspectionError,
            ".",
            "library_root_path_contains_symlink",
        );
        census.complete = false;
        return census;
    }
    let root_metadata = match inspect_metadata(root) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) => {
            report.finding(
                DiagnosticClass::InspectionError,
                ".",
                "library_root_not_directory",
            );
            census.complete = false;
            return census;
        }
        Err(reason) => {
            report.finding(DiagnosticClass::InspectionError, ".", reason);
            census.complete = false;
            return census;
        }
    };
    let _ = root_metadata;
    let mut total_entries = 0usize;
    let mut total_bytes = 0u64;
    census_walk(
        root,
        ".",
        0,
        &mut census,
        report,
        &mut total_entries,
        &mut total_bytes,
    );
    census
}

pub(crate) fn path_chain_contains_symlink(path: &Path) -> bool {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(current) => current.join(path),
            Err(_) => return true,
        }
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        if !matches!(component, std::path::Component::Normal(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return true,
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn census_walk(
    directory: &Path,
    relative: &str,
    depth: usize,
    census: &mut Census,
    report: &mut LibraryInspectionReport,
    total_entries: &mut usize,
    total_bytes: &mut u64,
) {
    if depth > MAX_TREE_DEPTH {
        report.finding(
            DiagnosticClass::InspectionError,
            relative.to_owned(),
            "traversal_depth_limit_exceeded",
        );
        census.complete = false;
        return;
    }
    let read_dir = match fs::read_dir(directory) {
        Ok(read_dir) => read_dir,
        Err(_) => {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                "directory_read_failed",
            );
            census.complete = false;
            return;
        }
    };
    let mut entries = Vec::new();
    for entry in read_dir {
        match entry {
            Ok(entry) => entries.push(entry),
            Err(_) => {
                report.finding(
                    DiagnosticClass::InspectionError,
                    relative.to_owned(),
                    "directory_entry_read_failed",
                );
                census.complete = false;
            }
        }
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        *total_entries = total_entries.saturating_add(1);
        if *total_entries > MAX_TREE_ENTRIES {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                "tree_entry_limit_exceeded",
            );
            census.complete = false;
            return;
        }
        let Some(name) = safe_component(&entry.file_name()) else {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                "unsafe_or_noncanonical_name",
            );
            census.complete = false;
            continue;
        };
        let Some(child_relative) = join_rel(relative, &name) else {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                "relative_path_limit_exceeded",
            );
            census.complete = false;
            continue;
        };
        let path = entry.path();
        let metadata = match inspect_metadata(&path) {
            Ok(metadata) => metadata,
            Err(reason) => {
                report.finding(DiagnosticClass::InspectionError, child_relative, reason);
                census.complete = false;
                continue;
            }
        };
        if metadata.is_dir() {
            census.entries.insert(
                child_relative.clone(),
                CensusEntry {
                    kind: "directory",
                    size: 0,
                    sha256: "-".into(),
                },
            );
            census_walk(
                &path,
                &child_relative,
                depth + 1,
                census,
                report,
                total_entries,
                total_bytes,
            );
        } else if metadata.is_file() {
            *total_bytes = total_bytes.saturating_add(metadata.len());
            if *total_bytes > MAX_TOTAL_BYTES_PER_PASS {
                report.finding(
                    DiagnosticClass::InspectionError,
                    child_relative,
                    "tree_byte_limit_exceeded",
                );
                census.complete = false;
                return;
            }
            match hash_file(&path, &metadata) {
                Ok(sha256) => {
                    census.entries.insert(
                        child_relative,
                        CensusEntry {
                            kind: "file",
                            size: metadata.len(),
                            sha256,
                        },
                    );
                }
                Err(reason) => {
                    report.finding(DiagnosticClass::InspectionError, child_relative, reason);
                    census.complete = false;
                }
            }
        } else {
            report.finding(
                DiagnosticClass::InspectionError,
                child_relative,
                "unsupported_special_file",
            );
            census.complete = false;
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ObservedLayout {
    notebooks: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug)]
struct PageContext {
    notebook_id: String,
    page_id: String,
    notebook_dir: PathBuf,
    relative_dir: String,
}

pub fn inspect_library(root: &Path) -> LibraryInspectionReport {
    inspect_library_with_hook(root, || {})
}

fn inspect_library_with_hook(root: &Path, after_scan: impl FnOnce()) -> LibraryInspectionReport {
    let mut report = LibraryInspectionReport::default();
    let before = collect_census(root, &mut report);
    if !before.complete {
        // Continue where possible so one unsafe object does not hide independent required files.
        report.completed = false;
    }

    if !path_chain_contains_symlink(root)
        && inspect_metadata(root).is_ok_and(|metadata| metadata.is_dir())
    {
        report.counts.libraries = 1;
        scan_library(root, &mut report);
    }

    after_scan();
    let after = collect_census(root, &mut report);
    if before.entries != after.entries || before.complete != after.complete {
        report.finding(
            DiagnosticClass::InspectionError,
            ".",
            "filesystem_changed_during_inspection",
        );
    }
    report.finish();
    report
}

fn scan_library(root: &Path, report: &mut LibraryInspectionReport) {
    let manifest = scan_library_manifest(root, report);
    let identity = read_identity(root, report);
    let layout = scan_notebooks(root, manifest.as_ref(), identity.as_ref(), report);
    scan_folders(root, manifest.as_ref(), report);
    scan_import_state(root, &layout, report);
    scan_library_sync_state(root, &layout, identity.as_ref(), report);
    scan_migration_state(root, report);
    scan_root_entries(root, manifest.as_ref(), report);
}

fn read_required(
    path: &Path,
    relative: &str,
    max_bytes: u64,
    report: &mut LibraryInspectionReport,
) -> Option<Vec<u8>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.finding(
                DiagnosticClass::MissingRequired,
                relative.to_owned(),
                "required_object_missing",
            );
            None
        }
        Err(_) => {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                "metadata_unavailable",
            );
            None
        }
        Ok(_) => match read_bounded(path, max_bytes) {
            Ok(bytes) => {
                report.counts.bytes_examined = report
                    .counts
                    .bytes_examined
                    .saturating_add(bytes.len() as u64);
                Some(bytes)
            }
            Err(reason) => {
                report.finding(
                    DiagnosticClass::InspectionError,
                    relative.to_owned(),
                    reason,
                );
                None
            }
        },
    }
}

fn read_optional(
    path: &Path,
    relative: &str,
    max_bytes: u64,
    report: &mut LibraryInspectionReport,
) -> Option<Vec<u8>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                "metadata_unavailable",
            );
            None
        }
        Ok(_) => match read_bounded(path, max_bytes) {
            Ok(bytes) => {
                report.counts.bytes_examined = report
                    .counts
                    .bytes_examined
                    .saturating_add(bytes.len() as u64);
                Some(bytes)
            }
            Err(reason) => {
                report.finding(
                    DiagnosticClass::InspectionError,
                    relative.to_owned(),
                    reason,
                );
                None
            }
        },
    }
}

fn strict_value(
    bytes: &[u8],
    relative: &str,
    report: &mut LibraryInspectionReport,
) -> Option<Value> {
    match parse_json_strict(bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            report.finding(
                DiagnosticClass::Corrupt,
                relative.to_owned(),
                json_reason(error),
            );
            None
        }
    }
}

fn scan_library_manifest(
    root: &Path,
    report: &mut LibraryInspectionReport,
) -> Option<PersistedLibraryManifestV1> {
    let relative = LIBRARY_MANIFEST_FILE;
    let bytes = read_required(
        &root.join(LIBRARY_MANIFEST_FILE),
        relative,
        MAX_METADATA_BYTES,
        report,
    )?;
    let value = strict_value(&bytes, relative, report)?;
    if object_has_unknown_fields(
        &value,
        &[
            "v",
            "notebook_ids",
            "active_notebook_id",
            "next_notebook_ordinal",
        ],
    ) {
        report.finding(
            DiagnosticClass::Unverified,
            relative,
            "unknown_library_manifest_fields",
        );
    }
    let version = value.get("v").and_then(Value::as_u64);
    if version != Some(1) {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative,
            "unsupported_library_manifest_version",
        );
        return None;
    }
    let manifest: PersistedLibraryManifestV1 = match serde_json::from_value(value) {
        Ok(manifest) => manifest,
        Err(_) => {
            report.finding(
                DiagnosticClass::Corrupt,
                relative,
                "invalid_library_manifest_shape",
            );
            return None;
        }
    };
    if validate_library_manifest(&manifest).is_err() {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "invalid_library_manifest_invariants",
        );
        return None;
    }
    if manifest.notebook_ids.len() > MAX_NOTEBOOKS {
        report.finding(
            DiagnosticClass::InspectionError,
            relative,
            "notebook_count_limit_exceeded",
        );
        return None;
    }
    report.observe(DiagnosticClass::Valid);
    Some(manifest)
}

fn scan_notebooks(
    root: &Path,
    manifest: Option<&PersistedLibraryManifestV1>,
    identity: Option<&IdentityFileV1>,
    report: &mut LibraryInspectionReport,
) -> ObservedLayout {
    let notebooks_path = root.join(NOTEBOOKS_DIR);
    match inspect_metadata(&notebooks_path) {
        Err("metadata_unavailable") => report.finding(
            DiagnosticClass::MissingRequired,
            NOTEBOOKS_DIR,
            "notebooks_directory_missing",
        ),
        Err(reason) => report.finding(DiagnosticClass::InspectionError, NOTEBOOKS_DIR, reason),
        Ok(metadata) if !metadata.is_dir() => report.finding(
            DiagnosticClass::InspectionError,
            NOTEBOOKS_DIR,
            "notebooks_object_not_directory",
        ),
        Ok(_) => report.observe(DiagnosticClass::Valid),
    }
    let mut layout = ObservedLayout::default();
    let Some(manifest) = manifest else {
        scan_unreferenced_notebook_dirs(&notebooks_path, &HashSet::new(), report);
        return layout;
    };
    let expected: HashSet<&str> = manifest.notebook_ids.iter().map(String::as_str).collect();
    for notebook_id in &manifest.notebook_ids {
        if !canonical_ordinal_id(notebook_id, "notebook-") {
            report.finding(
                DiagnosticClass::Corrupt,
                LIBRARY_MANIFEST_FILE,
                "noncanonical_notebook_identity",
            );
            continue;
        }
        let relative = format!("{NOTEBOOKS_DIR}/{notebook_id}");
        let directory = notebooks_path.join(notebook_id);
        let metadata = match inspect_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => metadata,
            Ok(_) => {
                report.finding(
                    DiagnosticClass::InspectionError,
                    relative,
                    "referenced_notebook_not_directory",
                );
                continue;
            }
            Err("metadata_unavailable") => {
                report.finding(
                    DiagnosticClass::MissingRequired,
                    relative,
                    "referenced_notebook_missing",
                );
                continue;
            }
            Err(reason) => {
                report.finding(DiagnosticClass::InspectionError, relative, reason);
                continue;
            }
        };
        let _ = metadata;
        report.counts.notebooks = report.counts.notebooks.saturating_add(1);
        scan_notebook_info(&directory, &relative, report);
        if let Some(notebook_manifest) =
            scan_notebook_manifest(&directory, &relative, notebook_id, report)
        {
            layout
                .notebooks
                .insert(notebook_id.clone(), notebook_manifest.page_ids.clone());
            scan_pages(
                &directory,
                &relative,
                notebook_id,
                &notebook_manifest,
                identity,
                report,
            );
        }
        scan_notebook_entries(
            &directory,
            &relative,
            layout.notebooks.get(notebook_id),
            report,
        );
    }
    scan_unreferenced_notebook_dirs(&notebooks_path, &expected, report);
    layout
}

fn scan_notebook_info(directory: &Path, relative: &str, report: &mut LibraryInspectionReport) {
    let relative = format!("{relative}/{NOTEBOOK_INFO_FILE}");
    let Some(bytes) = read_required(
        &directory.join(NOTEBOOK_INFO_FILE),
        &relative,
        MAX_METADATA_BYTES,
        report,
    ) else {
        return;
    };
    let Some(value) = strict_value(&bytes, &relative, report) else {
        return;
    };
    if object_has_unknown_fields(&value, &["v", "title"]) {
        report.finding(
            DiagnosticClass::Unverified,
            relative.clone(),
            "unknown_notebook_info_fields",
        );
    }
    if value.get("v").and_then(Value::as_u64) != Some(1) {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative,
            "unsupported_notebook_info_version",
        );
        return;
    }
    let info: PersistedNotebookInfoV1 = match serde_json::from_value(value) {
        Ok(info) => info,
        Err(_) => {
            report.finding(
                DiagnosticClass::Corrupt,
                relative,
                "invalid_notebook_info_shape",
            );
            return;
        }
    };
    if validate_title(&info.title).is_err() {
        report.finding(DiagnosticClass::Corrupt, relative, "invalid_notebook_title");
    } else {
        report.observe(DiagnosticClass::Valid);
    }
}

fn scan_notebook_manifest(
    directory: &Path,
    relative_dir: &str,
    expected_id: &str,
    report: &mut LibraryInspectionReport,
) -> Option<PersistedNotebookManifestV1> {
    let relative = format!("{relative_dir}/{MANIFEST_FILE}");
    let bytes = read_required(
        &directory.join(MANIFEST_FILE),
        &relative,
        MAX_METADATA_BYTES,
        report,
    )?;
    let value = strict_value(&bytes, &relative, report)?;
    if object_has_unknown_fields(
        &value,
        &[
            "v",
            "notebook_id",
            "page_ids",
            "active_page_id",
            "next_page_ordinal",
        ],
    ) {
        report.finding(
            DiagnosticClass::Unverified,
            relative.clone(),
            "unknown_notebook_manifest_fields",
        );
    }
    if value.get("v").and_then(Value::as_u64) != Some(1) {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative,
            "unsupported_notebook_manifest_version",
        );
        return None;
    }
    let manifest: PersistedNotebookManifestV1 = match serde_json::from_value(value) {
        Ok(manifest) => manifest,
        Err(_) => {
            report.finding(
                DiagnosticClass::Corrupt,
                relative,
                "invalid_notebook_manifest_shape",
            );
            return None;
        }
    };
    if validate_notebook_manifest(&manifest).is_err() || manifest.notebook_id != expected_id {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "invalid_notebook_manifest_invariants",
        );
        return None;
    }
    if manifest.page_ids.len() > MAX_PAGES {
        report.finding(
            DiagnosticClass::InspectionError,
            relative,
            "page_count_limit_exceeded",
        );
        return None;
    }
    if manifest
        .page_ids
        .iter()
        .any(|page_id| !canonical_ordinal_id(page_id, "page-"))
    {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "noncanonical_page_identity",
        );
        return None;
    }
    report.observe(DiagnosticClass::Valid);
    Some(manifest)
}

fn canonical_ordinal_id(value: &str, prefix: &str) -> bool {
    let Some(number) = value.strip_prefix(prefix) else {
        return false;
    };
    !number.is_empty()
        && !number.starts_with('0')
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && number.parse::<u64>().is_ok_and(|number| number > 0)
}

fn scan_pages(
    notebook_dir: &Path,
    notebook_relative: &str,
    notebook_id: &str,
    manifest: &PersistedNotebookManifestV1,
    identity: Option<&IdentityFileV1>,
    report: &mut LibraryInspectionReport,
) {
    for page_id in &manifest.page_ids {
        if report.counts.pages as usize >= MAX_PAGES {
            report.finding(
                DiagnosticClass::InspectionError,
                notebook_relative.to_owned(),
                "page_count_limit_exceeded",
            );
            return;
        }
        let relative_dir = format!("{notebook_relative}/{page_id}");
        let directory = notebook_dir.join(page_id);
        match inspect_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                report.finding(
                    DiagnosticClass::InspectionError,
                    relative_dir,
                    "referenced_page_not_directory",
                );
                continue;
            }
            Err("metadata_unavailable") => {
                report.finding(
                    DiagnosticClass::MissingRequired,
                    relative_dir,
                    "referenced_page_missing",
                );
                continue;
            }
            Err(reason) => {
                report.finding(DiagnosticClass::InspectionError, relative_dir, reason);
                continue;
            }
        }
        report.counts.pages = report.counts.pages.saturating_add(1);
        scan_page(
            PageContext {
                notebook_id: notebook_id.to_owned(),
                page_id: page_id.to_owned(),
                notebook_dir: notebook_dir.to_path_buf(),
                relative_dir,
            },
            identity,
            report,
        );
    }
}

fn scan_unreferenced_notebook_dirs(
    notebooks_dir: &Path,
    expected: &HashSet<&str>,
    report: &mut LibraryInspectionReport,
) {
    let Ok(read_dir) = fs::read_dir(notebooks_dir) else {
        return;
    };
    let mut names = Vec::new();
    for entry in read_dir.flatten() {
        if let Some(name) = safe_component(&entry.file_name()) {
            names.push((name, entry.path()));
        }
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path) in names {
        if expected.contains(name.as_str()) {
            continue;
        }
        let relative = format!("{NOTEBOOKS_DIR}/{name}");
        if name == MIGRATION_STAGE_NAME || is_temp_name(&name) {
            report.finding(
                DiagnosticClass::IncompleteTransaction,
                relative,
                "temporary_or_migration_notebook_present",
            );
        } else if inspect_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            report.finding(
                DiagnosticClass::Unverified,
                relative,
                "orphan_notebook_preserved_not_semantically_validated",
            );
        } else {
            report.finding(
                DiagnosticClass::Unverified,
                relative,
                "unknown_notebooks_object",
            );
        }
    }
}

fn scan_notebook_entries(
    directory: &Path,
    relative_dir: &str,
    page_ids: Option<&Vec<String>>,
    report: &mut LibraryInspectionReport,
) {
    let expected: HashSet<&str> = page_ids
        .map(|ids| ids.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let Ok(read_dir) = fs::read_dir(directory) else {
        return;
    };
    let mut names = read_dir
        .flatten()
        .filter_map(|entry| safe_component(&entry.file_name()).map(|name| (name, entry.path())))
        .collect::<Vec<_>>();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path) in names {
        if matches!(
            name.as_str(),
            NOTEBOOK_INFO_FILE | MANIFEST_FILE | ORIGIN_MARKER
        ) || expected.contains(name.as_str())
        {
            continue;
        }
        let relative = format!("{relative_dir}/{name}");
        if name == PAGE_TRASH_DIR {
            if inspect_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                report.finding(
                    DiagnosticClass::Unverified,
                    relative,
                    "page_trash_preserved_not_semantically_validated",
                );
            } else {
                report.finding(
                    DiagnosticClass::InspectionError,
                    relative,
                    "page_trash_not_directory",
                );
            }
        } else if is_temp_name(&name) {
            report.finding(
                DiagnosticClass::IncompleteTransaction,
                relative,
                "temporary_notebook_object_present",
            );
        } else if name.starts_with("page-")
            && inspect_metadata(&path).is_ok_and(|metadata| metadata.is_dir())
        {
            report.finding(
                DiagnosticClass::Unverified,
                relative,
                "orphan_page_preserved_not_semantically_validated",
            );
        } else if name.starts_with(".scribe-dev-") {
            report.finding(
                DiagnosticClass::Unverified,
                relative,
                "developer_trigger_present",
            );
        } else {
            report.finding(
                DiagnosticClass::Unverified,
                relative,
                "unknown_notebook_object",
            );
        }
    }
}

fn is_temp_name(name: &str) -> bool {
    name.ends_with(".tmp")
        || name.ends_with(".new")
        || name.ends_with(".partial")
        || name.contains(".migrating")
}

#[derive(Clone, Debug)]
struct HeaderInspection {
    header: PersistedPageHeader,
    profile: RenderProfile,
}

#[derive(Clone, Debug, Default)]
struct PageRecordState {
    visible_styles: HashMap<String, Option<String>>,
    known_ids: HashSet<String>,
    semantic_trusted: bool,
}

fn scan_page(
    context: PageContext,
    identity: Option<&IdentityFileV1>,
    report: &mut LibraryInspectionReport,
) {
    let directory = context.notebook_dir.join(&context.page_id);
    let header = scan_page_header(&directory, &context, report);
    scan_background(&directory, &context, header.as_ref(), report);
    let mut record_state = scan_strokes(
        &directory,
        &context,
        header.as_ref().map(|header| &header.profile),
        report,
    );
    scan_edits(
        &directory,
        &context,
        header.as_ref().map(|header| header.header.v),
        &mut record_state,
        report,
    );
    scan_texts(&directory, &context, report);
    scan_links(&directory, &context, identity, report);
    scan_page_entries(&directory, &context.relative_dir, report);
}

fn scan_page_header(
    directory: &Path,
    context: &PageContext,
    report: &mut LibraryInspectionReport,
) -> Option<HeaderInspection> {
    let relative = format!("{}/page.meta", context.relative_dir);
    let bytes = read_required(
        &directory.join("page.meta"),
        &relative,
        MAX_PAGE_META_BYTES,
        report,
    )?;
    let value = strict_value(&bytes, &relative, report)?;
    if object_has_unknown_fields(
        &value,
        &[
            "v",
            "page_id",
            "canonical_width",
            "canonical_height",
            "panel_width",
            "panel_height",
            "line_length",
            "background_bytes",
            "background_checksum",
            "bg_layout_version",
            "render",
            "edit_journal_version",
            "background_identity",
        ],
    ) {
        report.finding(
            DiagnosticClass::Unverified,
            relative.clone(),
            "unknown_page_header_fields",
        );
    }
    let Some(version_u64) = value.get("v").and_then(Value::as_u64) else {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "page_header_version_missing",
        );
        return None;
    };
    let Ok(version) = u32::try_from(version_u64) else {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative,
            "unsupported_page_header_version",
        );
        return None;
    };
    report.observed_page_formats.insert(version);
    if !matches!(
        version,
        FORMAT_VERSION_V2 | FORMAT_VERSION_V3 | FORMAT_VERSION_V4 | FORMAT_VERSION
    ) {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative,
            "unsupported_page_header_version",
        );
        return None;
    }
    let header: PersistedPageHeader = match serde_json::from_value(value) {
        Ok(header) => header,
        Err(_) => {
            report.finding(
                DiagnosticClass::Corrupt,
                relative,
                "invalid_page_header_shape",
            );
            return None;
        }
    };
    if header.page_id != context.page_id
        || (header.canonical_width - notebook_core::CANONICAL_WIDTH).abs() > f32::EPSILON
        || (header.canonical_height - notebook_core::CANONICAL_HEIGHT).abs() > f32::EPSILON
        || header.panel_width == 0
        || header.panel_height == 0
        || header.line_length < header.panel_width
        || header.bg_layout_version != BG_LAYOUT_VERSION
    {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "invalid_page_header_invariants",
        );
        return None;
    }
    let expected_edit_version = match version {
        FORMAT_VERSION_V2 => 0,
        FORMAT_VERSION_V3 | FORMAT_VERSION_V4 => EDIT_JOURNAL_VERSION_V1,
        FORMAT_VERSION => EDIT_JOURNAL_VERSION,
        _ => unreachable!(),
    };
    if header.edit_journal_version != expected_edit_version {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "page_edit_journal_version_mismatch",
        );
        return None;
    }
    let profile = validate_header_profile(&header, &relative, report)?;
    if version != FORMAT_VERSION {
        report.finding(
            DiagnosticClass::Valid,
            relative.clone(),
            "normal_open_action=migrate-to-v5",
        );
    } else {
        report.observe(DiagnosticClass::Valid);
    }
    Some(HeaderInspection { header, profile })
}

fn validate_header_profile(
    header: &PersistedPageHeader,
    relative: &str,
    report: &mut LibraryInspectionReport,
) -> Option<RenderProfile> {
    let expected_v1 = matches!(header.v, FORMAT_VERSION_V2 | FORMAT_VERSION_V3);
    let allowed_v1 = [
        "profile_version",
        "ink_byte",
        "brush_radius",
        "transform",
        "rasterizer",
    ];
    let allowed_v2 = [
        "profile_version",
        "ink_byte",
        "legacy_fixed_radius",
        "pressure_min_radius",
        "pressure_max_radius",
        "pressure_missing_fallback_radius",
        "pressure_curve",
        "pressure_q_r1_max",
        "pressure_q_r2_max",
        "pressure_q_r3_max",
        "transform",
        "rasterizer",
    ];
    if object_has_unknown_fields(
        &header.render,
        if expected_v1 {
            &allowed_v1
        } else {
            &allowed_v2
        },
    ) {
        report.finding(
            DiagnosticClass::Unverified,
            relative.to_owned(),
            "unknown_render_profile_fields",
        );
    }
    if expected_v1 {
        if header.render.get("profile_version").and_then(Value::as_u64)
            != Some(u64::from(RENDER_PROFILE_VERSION_V1))
        {
            report.finding(
                DiagnosticClass::UnsupportedFormat,
                relative.to_owned(),
                "unsupported_render_profile_version",
            );
            return None;
        }
        let profile: PersistedRenderProfileV1 = match serde_json::from_value(header.render.clone())
        {
            Ok(profile) => profile,
            Err(_) => {
                report.finding(
                    DiagnosticClass::Corrupt,
                    relative.to_owned(),
                    "invalid_render_profile_shape",
                );
                return None;
            }
        };
        return match validate_persisted_render_profile_v1(&profile) {
            Ok(profile) => Some(profile),
            Err(_) => {
                report.finding(
                    DiagnosticClass::Corrupt,
                    relative.to_owned(),
                    "invalid_render_profile_invariants",
                );
                None
            }
        };
    }
    if header.render.get("profile_version").and_then(Value::as_u64)
        != Some(u64::from(RENDER_PROFILE_VERSION))
    {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative.to_owned(),
            "unsupported_render_profile_version",
        );
        return None;
    }
    let mut render = header.render.clone();
    if header.v == FORMAT_VERSION_V4 && render.get("pressure_missing_fallback_radius").is_none() {
        match crate::store::normalize_historical_v4_render_for_migration(&render) {
            Ok(normalized) => {
                render = normalized;
                report.finding(
                    DiagnosticClass::Valid,
                    relative.to_owned(),
                    "normal_open_action=normalize-v4-render-profile",
                );
            }
            Err(_) => {
                report.finding(
                    DiagnosticClass::Corrupt,
                    relative.to_owned(),
                    "invalid_render_profile_shape",
                );
                return None;
            }
        }
    }
    let profile: PersistedRenderProfileV2 = match serde_json::from_value(render) {
        Ok(profile) => profile,
        Err(_) => {
            report.finding(
                DiagnosticClass::Corrupt,
                relative.to_owned(),
                "invalid_render_profile_shape",
            );
            return None;
        }
    };
    match validate_persisted_render_profile_v2(&profile) {
        Ok(profile) => Some(profile),
        Err(_) => {
            report.finding(
                DiagnosticClass::Corrupt,
                relative.to_owned(),
                "invalid_render_profile_invariants",
            );
            None
        }
    }
}

fn scan_background(
    directory: &Path,
    context: &PageContext,
    inspected: Option<&HeaderInspection>,
    report: &mut LibraryInspectionReport,
) {
    let relative = format!("{}/background.raw", context.relative_dir);
    let Some(bytes) = read_required(
        &directory.join("background.raw"),
        &relative,
        MAX_BACKGROUND_BYTES,
        report,
    ) else {
        return;
    };
    let Some(inspected) = inspected else {
        report.finding(
            DiagnosticClass::Unverified,
            relative,
            "background_header_unavailable",
        );
        return;
    };
    let header = &inspected.header;
    if bytes.len() as u64 != header.background_bytes
        || rect_checksum(&bytes) != header.background_checksum
    {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "background_metadata_mismatch",
        );
        return;
    }
    let expected_packed = u64::from(header.panel_width) * u64::from(header.panel_height);
    let expected_stride = u64::from(header.line_length) * u64::from(header.panel_height);
    if header.background_bytes != expected_packed && header.background_bytes != expected_stride {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "background_geometry_mismatch",
        );
        return;
    }
    match header.background_identity.as_ref() {
        None => report.finding(
            DiagnosticClass::Unverified,
            format!("{}/page.meta", context.relative_dir),
            "normal_open_action=seal-background-identity",
        ),
        Some(identity) => {
            let valid = identity.validate().is_ok()
                && match identity {
                    BackgroundIdentityV1::Blank { .. } => bytes.iter().all(|byte| *byte == 0xff),
                    BackgroundIdentityV1::Blob { sha256, .. } => {
                        format!("{:x}", Sha256::digest(&bytes)) == *sha256
                    }
                };
            if !valid {
                report.finding(
                    DiagnosticClass::Unverified,
                    format!("{}/page.meta", context.relative_dir),
                    "normal_open_action=repair-background-identity",
                );
            }
        }
    }
    report.observe(DiagnosticClass::Valid);
}

fn scan_strokes(
    directory: &Path,
    context: &PageContext,
    profile: Option<&RenderProfile>,
    report: &mut LibraryInspectionReport,
) -> PageRecordState {
    let relative = format!("{}/strokes.jsonl", context.relative_dir);
    let mut state = PageRecordState {
        semantic_trusted: true,
        ..PageRecordState::default()
    };
    scan_jsonl(
        &directory.join("strokes.jsonl"),
        &relative,
        true,
        report,
        |line, report| {
            report.counts.stroke_records = report.counts.stroke_records.saturating_add(1);
            let Some(value) = strict_value(line, &relative, report) else {
                state.semantic_trusted = false;
                return;
            };
            let Some(version) = value.get("v").and_then(Value::as_u64) else {
                report.finding_once(
                    DiagnosticClass::Corrupt,
                    relative.clone(),
                    "stroke_record_version_missing",
                );
                state.semantic_trusted = false;
                return;
            };
            if !matches!(version, 1 | 2) {
                report.finding_once(
                    DiagnosticClass::UnsupportedFormat,
                    relative.clone(),
                    "unsupported_stroke_record_version",
                );
                state.semantic_trusted = false;
                return;
            }
            if stroke_value_has_unknown_fields(&value, version) {
                report.finding_once(
                    DiagnosticClass::Unverified,
                    relative.clone(),
                    "unknown_stroke_record_fields",
                );
            }
            let stroke = match parse_stroke_record(line) {
                Ok(stroke) => stroke,
                Err(_) => {
                    report.finding_once(
                        DiagnosticClass::Corrupt,
                        relative.clone(),
                        "invalid_stroke_record",
                    );
                    state.semantic_trusted = false;
                    return;
                }
            };
            if stroke.points.len() > MAX_POINTS_PER_STROKE
                || stroke
                    .points
                    .iter()
                    .any(|point| validate_persisted_canonical_xy(point.x, point.y).is_err())
                || (stroke.brush.as_deref() == Some(STYLE_PRESSURE_WIDTH_V1)
                    && profile.is_some_and(|profile| profile.pressure.is_none()))
            {
                report.finding_once(
                    DiagnosticClass::Corrupt,
                    relative.clone(),
                    "invalid_stroke_semantics",
                );
                state.semantic_trusted = false;
                return;
            }
            let id = stroke.id.as_str().to_owned();
            if !state.known_ids.insert(id.clone()) {
                report.finding_once(
                    DiagnosticClass::Corrupt,
                    relative.clone(),
                    "duplicate_stroke_identity",
                );
                state.semantic_trusted = false;
                return;
            }
            state.visible_styles.insert(id, stroke.brush);
            report.observe(DiagnosticClass::Valid);
        },
    );
    state
}

fn stroke_value_has_unknown_fields(value: &Value, version: u64) -> bool {
    let top = if version == 1 {
        &["v", "type", "id", "finished", "points"][..]
    } else {
        &["v", "type", "id", "finished", "style", "points"][..]
    };
    object_has_unknown_fields(value, top)
        || value
            .get("points")
            .and_then(Value::as_array)
            .is_some_and(|points| {
                points
                    .iter()
                    .any(|point| object_has_unknown_fields(point, &["x", "y", "pressure"]))
            })
}

fn scan_edits(
    directory: &Path,
    context: &PageContext,
    page_version: Option<u32>,
    state: &mut PageRecordState,
    report: &mut LibraryInspectionReport,
) {
    let relative = format!("{}/edits.jsonl", context.relative_dir);
    if page_version == Some(FORMAT_VERSION_V2) {
        if let Some(bytes) = read_optional(
            &directory.join("edits.jsonl"),
            &relative,
            MAX_JOURNAL_BYTES,
            report,
        ) {
            let reason = if bytes.is_empty() {
                "v2_precreated_edit_journal"
            } else {
                "v2_unexpected_edit_journal_content"
            };
            report.finding(DiagnosticClass::IncompleteTransaction, relative, reason);
        }
        return;
    }
    let required = page_version.is_some();
    scan_jsonl(
        &directory.join("edits.jsonl"),
        &relative,
        required,
        report,
        |line, report| {
            report.counts.edit_records = report.counts.edit_records.saturating_add(1);
            let Some(value) = strict_value(line, &relative, report) else {
                state.semantic_trusted = false;
                return;
            };
            let version = value.get("v").and_then(Value::as_u64);
            let record_type = value.get("type").and_then(Value::as_str);
            if !matches!(
                (version, record_type),
                (Some(1), Some("delete_stroke")) | (Some(2), Some("replace_strokes"))
            ) {
                report.finding_once(
                    DiagnosticClass::UnsupportedFormat,
                    relative.clone(),
                    "unsupported_edit_record_variant",
                );
                state.semantic_trusted = false;
                return;
            }
            if matches!(
                page_version,
                Some(FORMAT_VERSION_V3) | Some(FORMAT_VERSION_V4)
            ) && version == Some(2)
            {
                report.finding_once(
                    DiagnosticClass::Corrupt,
                    relative.clone(),
                    "edit_record_exceeds_page_header_contract",
                );
                state.semantic_trusted = false;
                return;
            }
            if edit_value_has_unknown_fields(&value, version.unwrap_or_default()) {
                report.finding_once(
                    DiagnosticClass::Unverified,
                    relative.clone(),
                    "unknown_edit_record_fields",
                );
            }
            let operation = match parse_edit_record_line(line) {
                Ok(operation) => operation,
                Err(_) => {
                    report.finding_once(
                        DiagnosticClass::Corrupt,
                        relative.clone(),
                        "invalid_edit_record",
                    );
                    state.semantic_trusted = false;
                    return;
                }
            };
            if state.semantic_trusted && apply_edit_for_inspection(state, operation).is_err() {
                report.finding_once(
                    DiagnosticClass::Corrupt,
                    relative.clone(),
                    "invalid_edit_record_semantics",
                );
                state.semantic_trusted = false;
                return;
            }
            report.observe(DiagnosticClass::Valid);
        },
    );
}

fn edit_value_has_unknown_fields(value: &Value, version: u64) -> bool {
    if version == 1 {
        return object_has_unknown_fields(value, &["v", "type", "stroke_id"]);
    }
    if object_has_unknown_fields(value, &["v", "type", "replacements"]) {
        return true;
    }
    value
        .get("replacements")
        .and_then(Value::as_array)
        .is_some_and(|replacements| {
            replacements.iter().any(|replacement| {
                object_has_unknown_fields(replacement, &["target_stroke_id", "fragments"])
                    || replacement
                        .get("fragments")
                        .and_then(Value::as_array)
                        .is_some_and(|fragments| {
                            fragments.iter().any(|fragment| {
                                object_has_unknown_fields(fragment, &["id", "style", "points"])
                                    || fragment
                                        .get("points")
                                        .and_then(Value::as_array)
                                        .is_some_and(|points| {
                                            points.iter().any(|point| {
                                                object_has_unknown_fields(
                                                    point,
                                                    &["x", "y", "pressure"],
                                                )
                                            })
                                        })
                            })
                        })
            })
        })
}

fn apply_edit_for_inspection(state: &mut PageRecordState, operation: EditOp) -> Result<(), ()> {
    match operation {
        EditOp::DeleteStroke { stroke_id } => {
            if state.visible_styles.remove(&stroke_id).is_none() {
                return Err(());
            }
        }
        EditOp::ReplaceStrokes { replacements } => {
            validate_replace_strokes_structural(&replacements).map_err(|_| ())?;
            prepare_replacement_identity_changes(state, &replacements)?;
        }
    }
    Ok(())
}

fn prepare_replacement_identity_changes(
    state: &mut PageRecordState,
    replacements: &[PersistedReplacementV2],
) -> Result<(), ()> {
    let mut targets = HashSet::new();
    let mut fragments = HashSet::new();
    for replacement in replacements {
        if !targets.insert(replacement.target_stroke_id.clone()) {
            return Err(());
        }
        let target_style = state
            .visible_styles
            .get(&replacement.target_stroke_id)
            .ok_or(())?;
        for fragment in &replacement.fragments {
            let preserves_target =
                replacement.fragments.len() == 1 && fragment.id == replacement.target_stroke_id;
            if preserves_target && &fragment.style != target_style {
                return Err(());
            }
            if (!preserves_target && state.known_ids.contains(&fragment.id))
                || !fragments.insert(fragment.id.clone())
            {
                return Err(());
            }
        }
    }
    for replacement in replacements {
        state.visible_styles.remove(&replacement.target_stroke_id);
        for fragment in &replacement.fragments {
            state.known_ids.insert(fragment.id.clone());
            state
                .visible_styles
                .insert(fragment.id.clone(), fragment.style.clone());
        }
    }
    Ok(())
}

fn scan_texts(directory: &Path, context: &PageContext, report: &mut LibraryInspectionReport) {
    let relative = format!("{}/texts.jsonl", context.relative_dir);
    let mut ids = HashSet::new();
    scan_jsonl(
        &directory.join("texts.jsonl"),
        &relative,
        false,
        report,
        |line, report| {
            report.counts.text_records = report.counts.text_records.saturating_add(1);
            let Some(value) = strict_value(line, &relative, report) else {
                return;
            };
            if object_has_unknown_fields(
                &value,
                &["text_id", "x", "y", "width", "font_size", "face", "content"],
            ) {
                report.finding_once(
                    DiagnosticClass::UnsupportedFormat,
                    relative.clone(),
                    "unknown_text_record_fields",
                );
                return;
            }
            let text: sync_protocol::SyncText = match serde_json::from_value(value) {
                Ok(text) => text,
                Err(_) => {
                    report.finding_once(
                        DiagnosticClass::Corrupt,
                        relative.clone(),
                        "invalid_text_record",
                    );
                    return;
                }
            };
            if text.validate().is_err() || !ids.insert(text.text_id.to_string()) {
                report.finding_once(
                    DiagnosticClass::Corrupt,
                    relative.clone(),
                    "invalid_text_record_semantics",
                );
            } else {
                report.observe(DiagnosticClass::Valid);
            }
        },
    );
}

fn scan_links(
    directory: &Path,
    context: &PageContext,
    identity: Option<&IdentityFileV1>,
    report: &mut LibraryInspectionReport,
) {
    let relative = format!("{}/links.jsonl", context.relative_dir);
    let mut ids = HashSet::new();
    scan_jsonl(
        &directory.join("links.jsonl"),
        &relative,
        false,
        report,
        |line, report| {
            report.counts.link_records = report.counts.link_records.saturating_add(1);
            let Some(value) = strict_value(line, &relative, report) else {
                return;
            };
            if object_has_unknown_fields(
                &value,
                &[
                    "link_id",
                    "source_notebook_id",
                    "source_page_id",
                    "source_x",
                    "source_y",
                    "target_notebook_id",
                    "target_page_id",
                ],
            ) {
                report.finding_once(
                    DiagnosticClass::UnsupportedFormat,
                    relative.clone(),
                    "unknown_link_record_fields",
                );
                return;
            }
            let link: sync_protocol::SyncPageLink = match serde_json::from_value(value) {
                Ok(link) => link,
                Err(_) => {
                    report.finding_once(
                        DiagnosticClass::Corrupt,
                        relative.clone(),
                        "invalid_link_record",
                    );
                    return;
                }
            };
            if link.validate().is_err() || !ids.insert(link.link_id.to_string()) {
                report.finding_once(
                    DiagnosticClass::Corrupt,
                    relative.clone(),
                    "invalid_link_record_semantics",
                );
            } else {
                match identity.and_then(|identity| {
                    Some((
                        identity.notebook_id(&context.notebook_id)?,
                        identity.page_id(&context.notebook_id, &context.page_id)?,
                    ))
                }) {
                    Some((notebook_id, page_id))
                        if link.source_notebook_id == notebook_id
                            && link.source_page_id == page_id =>
                    {
                        report.observe(DiagnosticClass::Valid);
                    }
                    Some(_) => report.finding_once(
                        DiagnosticClass::Corrupt,
                        relative.clone(),
                        "link_source_identity_mismatch",
                    ),
                    None => report.finding_once(
                        DiagnosticClass::Unverified,
                        relative.clone(),
                        "link_source_identity_unavailable",
                    ),
                }
            }
        },
    );
}

fn scan_jsonl(
    path: &Path,
    relative: &str,
    required: bool,
    report: &mut LibraryInspectionReport,
    mut inspect_record: impl FnMut(&[u8], &mut LibraryInspectionReport),
) {
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if required {
                report.finding(
                    DiagnosticClass::MissingRequired,
                    relative.to_owned(),
                    "required_journal_missing",
                );
            }
            return;
        }
        Err(_) => {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                "metadata_unavailable",
            );
            return;
        }
        Ok(metadata) => metadata,
    };
    if !metadata.is_file() {
        report.finding(
            DiagnosticClass::InspectionError,
            relative.to_owned(),
            "journal_not_regular_file",
        );
        return;
    }
    if metadata.len() > MAX_JOURNAL_BYTES {
        report.finding(
            DiagnosticClass::InspectionError,
            relative.to_owned(),
            "journal_size_limit_exceeded",
        );
        return;
    }
    let file = match open_regular_read_only(path, &metadata) {
        Ok(file) => file,
        Err(reason) => {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                reason,
            );
            return;
        }
    };
    let mut reader = BufReader::new(file);
    let mut consumed = 0u64;
    let mut line = Vec::new();
    let mut stopped_at_limit = false;
    loop {
        line.clear();
        let limit = MAX_RECORD_BYTES.saturating_add(2);
        let count = match reader.by_ref().take(limit).read_until(b'\n', &mut line) {
            Ok(count) => count,
            Err(_) => {
                report.finding(
                    DiagnosticClass::InspectionError,
                    relative.to_owned(),
                    "journal_read_failed",
                );
                return;
            }
        };
        if count == 0 {
            break;
        }
        consumed = consumed.saturating_add(count as u64);
        report.counts.bytes_examined = report.counts.bytes_examined.saturating_add(count as u64);
        if line.last() != Some(&b'\n') {
            if count as u64 >= limit {
                report.finding(
                    DiagnosticClass::InspectionError,
                    relative.to_owned(),
                    "journal_record_size_limit_exceeded",
                );
                stopped_at_limit = true;
            } else {
                report.finding(
                    DiagnosticClass::IncompleteTransaction,
                    relative.to_owned(),
                    "unterminated_journal_tail",
                );
            }
            break;
        }
        line.pop();
        if line.len() as u64 > MAX_RECORD_BYTES {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                "journal_record_size_limit_exceeded",
            );
            stopped_at_limit = true;
            break;
        }
        if line.last() == Some(&b'\r') {
            line.pop();
            report.finding_once(
                DiagnosticClass::Unverified,
                relative.to_owned(),
                "noncanonical_crlf_journal",
            );
        }
        if line.is_empty() {
            report.finding_once(
                DiagnosticClass::Unverified,
                relative.to_owned(),
                "empty_journal_record",
            );
            continue;
        }
        let records = report
            .counts
            .stroke_records
            .saturating_add(report.counts.edit_records)
            .saturating_add(report.counts.text_records)
            .saturating_add(report.counts.link_records);
        if records >= MAX_RECORDS {
            report.finding(
                DiagnosticClass::InspectionError,
                relative.to_owned(),
                "record_count_limit_exceeded",
            );
            stopped_at_limit = true;
            break;
        }
        inspect_record(&line, report);
    }
    if !stopped_at_limit && consumed != metadata.len() {
        report.finding_once(
            DiagnosticClass::InspectionError,
            relative.to_owned(),
            "journal_changed_during_read",
        );
    } else if !stopped_at_limit {
        report.observe(DiagnosticClass::Valid);
    }
}

fn scan_page_entries(directory: &Path, relative_dir: &str, report: &mut LibraryInspectionReport) {
    const KNOWN: &[&str] = &[
        "page.meta",
        "background.raw",
        "strokes.jsonl",
        "edits.jsonl",
        "texts.jsonl",
        "links.jsonl",
    ];
    let Ok(read_dir) = fs::read_dir(directory) else {
        return;
    };
    let mut names = read_dir
        .flatten()
        .filter_map(|entry| safe_component(&entry.file_name()).map(|name| (name, entry.path())))
        .collect::<Vec<_>>();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, _) in names {
        if KNOWN.contains(&name.as_str()) {
            continue;
        }
        let relative = format!("{relative_dir}/{name}");
        if is_temp_name(&name) {
            report.finding(
                DiagnosticClass::IncompleteTransaction,
                relative,
                "temporary_page_object_present",
            );
        } else {
            report.finding(DiagnosticClass::Unverified, relative, "unknown_page_object");
        }
    }
}

fn scan_folders(
    root: &Path,
    manifest: Option<&PersistedLibraryManifestV1>,
    report: &mut LibraryInspectionReport,
) {
    let directory = root.join(FOLDERS_DIR);
    let metadata = match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            report.finding(
                DiagnosticClass::InspectionError,
                FOLDERS_DIR,
                "metadata_unavailable",
            );
            return;
        }
        Ok(metadata) => metadata,
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        report.finding(
            DiagnosticClass::InspectionError,
            FOLDERS_DIR,
            "folders_object_not_directory",
        );
        return;
    }
    let relative = format!("{FOLDERS_DIR}/{FOLDERS_FILE}");
    let Some(bytes) = read_optional(
        &directory.join(FOLDERS_FILE),
        &relative,
        MAX_METADATA_BYTES,
        report,
    ) else {
        scan_aux_directory_entries(&directory, FOLDERS_DIR, &[], report);
        return;
    };
    let Some(value) = strict_value(&bytes, &relative, report) else {
        scan_aux_directory_entries(&directory, FOLDERS_DIR, &[FOLDERS_FILE], report);
        return;
    };
    if value.get("v").and_then(Value::as_u64) != Some(1) {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative.clone(),
            "unsupported_folders_version",
        );
    } else {
        let folders: Result<FoldersFileV1, _> = serde_json::from_value(value);
        match folders {
            Ok(folders) if crate::folders::validate(&folders).is_ok() => {
                let live = manifest
                    .map(|manifest| manifest.notebook_ids.iter().collect::<HashSet<_>>())
                    .unwrap_or_default();
                for folder in &folders.folders {
                    report.counts.folder_records = report.counts.folder_records.saturating_add(1);
                    if !canonical_ordinal_id(&folder.id, "folder-") {
                        report.finding_once(
                            DiagnosticClass::Corrupt,
                            relative.clone(),
                            "noncanonical_folder_identity",
                        );
                    }
                    if folder
                        .notebook_ids
                        .iter()
                        .any(|notebook| !live.contains(notebook))
                    {
                        report.finding_once(
                            DiagnosticClass::Unverified,
                            relative.clone(),
                            "folder_references_nonlive_notebook",
                        );
                    }
                }
                report.observe(DiagnosticClass::Valid);
            }
            _ => report.finding(
                DiagnosticClass::Corrupt,
                relative.clone(),
                "invalid_folders_file",
            ),
        }
    }
    scan_aux_directory_entries(&directory, FOLDERS_DIR, &[FOLDERS_FILE], report);
}

fn scan_import_state(root: &Path, layout: &ObservedLayout, report: &mut LibraryInspectionReport) {
    let directory = root.join(IMPORT_DIR);
    let receipts = match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            report.finding(
                DiagnosticClass::InspectionError,
                IMPORT_DIR,
                "metadata_unavailable",
            );
            None
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            report.finding(
                DiagnosticClass::InspectionError,
                IMPORT_DIR,
                "import_state_not_directory",
            );
            None
        }
        Ok(_) => {
            let receipts = scan_receipts(root, layout, report);
            scan_aux_directory_entries(&directory, IMPORT_DIR, &[RECEIPTS_FILE], report);
            receipts
        }
    };
    for notebook_id in layout.notebooks.keys() {
        let origin = scan_import_origin(root, notebook_id, report);
        match origin {
            Some(origin) => {
                let bound = receipts
                    .as_ref()
                    .and_then(|receipts| receipts.receipts.get(&receipt_key(&origin.origin_id)));
                if !bound.is_some_and(|receipt| {
                    receipt.origin_kind == origin.origin_kind
                        && receipt.dest_notebook_id.as_deref() == Some(notebook_id.as_str())
                }) {
                    report.finding(
                        DiagnosticClass::Corrupt,
                        format!("{NOTEBOOKS_DIR}/{notebook_id}/{ORIGIN_MARKER}"),
                        "import_origin_receipt_mismatch",
                    );
                }
            }
            None => {
                if receipts.as_ref().is_some_and(|receipts| {
                    receipts.receipts.values().any(|receipt| {
                        receipt.dest_notebook_id.as_deref() == Some(notebook_id.as_str())
                            && matches!(
                                receipt.state,
                                ReceiptState::Pending | ReceiptState::Complete
                            )
                    })
                }) {
                    report.finding(
                        DiagnosticClass::MissingRequired,
                        format!("{NOTEBOOKS_DIR}/{notebook_id}/{ORIGIN_MARKER}"),
                        "import_origin_missing_for_live_destination",
                    );
                }
            }
        }
    }
}

fn scan_receipts(
    root: &Path,
    layout: &ObservedLayout,
    report: &mut LibraryInspectionReport,
) -> Option<ReceiptsV1> {
    let relative = format!("{IMPORT_DIR}/{RECEIPTS_FILE}");
    let bytes = read_optional(
        &root.join(IMPORT_DIR).join(RECEIPTS_FILE),
        &relative,
        MAX_METADATA_BYTES,
        report,
    )?;
    let value = strict_value(&bytes, &relative, report)?;
    if value.get("v").and_then(Value::as_u64) != Some(1) {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative,
            "unsupported_import_receipts_version",
        );
        return None;
    }
    let receipts: ReceiptsV1 = match serde_json::from_value(value) {
        Ok(receipts) => receipts,
        Err(_) => {
            report.finding(
                DiagnosticClass::UnsupportedFormat,
                relative,
                "unknown_or_invalid_import_receipts_fields",
            );
            return None;
        }
    };
    let mut valid = true;
    for (key, receipt) in &receipts.receipts {
        report.counts.import_receipts = report.counts.import_receipts.saturating_add(1);
        let base_valid = key == &receipt_key(&receipt.origin_id)
            && receipt.origin_kind == ORIGIN_KIND
            && uuid::Uuid::parse_str(&receipt.origin_id).is_ok()
            && validate_title(&receipt.title).is_ok()
            && receipt
                .dest_notebook_id
                .as_deref()
                .is_none_or(|id| canonical_ordinal_id(id, "notebook-"));
        let state_valid = match receipt.state {
            ReceiptState::Pending => {
                receipt.dest_notebook_id.is_some()
                    && receipt.finished_unix_ms.is_none()
                    && receipt.pages.is_none()
                    && receipt.strokes.is_none()
            }
            ReceiptState::Complete => {
                receipt.finished_unix_ms.is_some()
                    && receipt.pages.is_some()
                    && receipt.strokes.is_some()
                    && receipt.error.is_none()
            }
            ReceiptState::Failed => receipt.finished_unix_ms.is_some(),
        };
        if !base_valid || !state_valid {
            valid = false;
            report.finding_once(
                DiagnosticClass::Corrupt,
                relative.clone(),
                "invalid_import_receipt_invariants",
            );
        }
        if receipt.state == ReceiptState::Pending {
            report.finding_once(
                DiagnosticClass::IncompleteTransaction,
                relative.clone(),
                "pending_import_receipt",
            );
            if receipt
                .dest_notebook_id
                .as_ref()
                .is_some_and(|id| !layout.notebooks.contains_key(id))
            {
                report.finding_once(
                    DiagnosticClass::Unverified,
                    relative.clone(),
                    "pending_import_destination_not_live",
                );
            }
        }
    }
    if valid {
        report.observe(DiagnosticClass::Valid);
    }
    Some(receipts)
}

fn scan_import_origin(
    root: &Path,
    notebook_id: &str,
    report: &mut LibraryInspectionReport,
) -> Option<ImportOriginV1> {
    let relative = format!("{NOTEBOOKS_DIR}/{notebook_id}/{ORIGIN_MARKER}");
    let path = root
        .join(NOTEBOOKS_DIR)
        .join(notebook_id)
        .join(ORIGIN_MARKER);
    let bytes = read_optional(&path, &relative, MAX_METADATA_BYTES, report)?;
    let value = strict_value(&bytes, &relative, report)?;
    if value.get("v").and_then(Value::as_u64) != Some(1) {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative,
            "unsupported_import_origin_version",
        );
        return None;
    }
    let origin: ImportOriginV1 = match serde_json::from_value(value) {
        Ok(origin) => origin,
        Err(_) => {
            report.finding(
                DiagnosticClass::UnsupportedFormat,
                relative,
                "unknown_or_invalid_import_origin_fields",
            );
            return None;
        }
    };
    if origin.origin_kind != ORIGIN_KIND || uuid::Uuid::parse_str(&origin.origin_id).is_err() {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "invalid_import_origin_invariants",
        );
    } else {
        report.observe(DiagnosticClass::Valid);
    }
    Some(origin)
}

fn scan_library_sync_state(
    root: &Path,
    layout: &ObservedLayout,
    identity: Option<&IdentityFileV1>,
    report: &mut LibraryInspectionReport,
) {
    let directory = root.join(SYNC_DIR);
    let metadata = match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            report.finding(
                DiagnosticClass::InspectionError,
                SYNC_DIR,
                "metadata_unavailable",
            );
            return;
        }
        Ok(metadata) => metadata,
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        report.finding(
            DiagnosticClass::InspectionError,
            SYNC_DIR,
            "library_sync_state_not_directory",
        );
        return;
    }
    validate_identity_layout(layout, identity, report);
    let Ok(read_dir) = fs::read_dir(&directory) else {
        return;
    };
    let mut names = read_dir
        .flatten()
        .filter_map(|entry| safe_component(&entry.file_name()).map(|name| (name, entry.path())))
        .collect::<Vec<_>>();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path) in names {
        if name == IDENTITY_FILE {
            continue;
        }
        let relative = format!("{SYNC_DIR}/{name}");
        match name.as_str() {
            IDENTITY_LOCK_FILE => report.finding(
                DiagnosticClass::Valid,
                relative,
                "lock_file_bytes_not_authoritative",
            ),
            crate::sync_verified::RECOVERY_MAINTENANCE_FILE => report.finding(
                DiagnosticClass::IncompleteTransaction,
                relative,
                "recovery_maintenance_active",
            ),
            crate::sync_verified::MUTATION_GEN_FILE => {
                scan_mutation_generation(&path, &relative, report)
            }
            crate::page_ledger::LEDGER_FILE => report.finding(
                DiagnosticClass::Unverified,
                relative,
                "page_ledger_semantics_not_part_of_library_probe",
            ),
            crate::nav_state::NAV_FILE | crate::nav_state::ACTIVE_NOTEBOOK_FILE => report.finding(
                DiagnosticClass::Unverified,
                relative,
                "navigation_state_not_semantically_validated",
            ),
            _ if is_temp_name(&name) => report.finding(
                DiagnosticClass::IncompleteTransaction,
                relative,
                "temporary_library_sync_object_present",
            ),
            _ => report.finding(
                DiagnosticClass::Unverified,
                relative,
                "unknown_library_sync_object",
            ),
        }
    }
}

fn read_identity(root: &Path, report: &mut LibraryInspectionReport) -> Option<IdentityFileV1> {
    let relative = format!("{SYNC_DIR}/{IDENTITY_FILE}");
    match fs::symlink_metadata(root.join(SYNC_DIR)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) | Err(_) => return None,
    }
    let bytes = read_optional(
        &root.join(SYNC_DIR).join(IDENTITY_FILE),
        &relative,
        MAX_METADATA_BYTES,
        report,
    )?;
    let value = strict_value(&bytes, &relative, report)?;
    if value.get("v").and_then(Value::as_u64) != Some(1) {
        report.finding(
            DiagnosticClass::UnsupportedFormat,
            relative,
            "unsupported_identity_version",
        );
        return None;
    }
    let identity: IdentityFileV1 = match serde_json::from_value(value) {
        Ok(identity) => identity,
        Err(_) => {
            report.finding(
                DiagnosticClass::UnsupportedFormat,
                relative,
                "unknown_or_invalid_identity_fields",
            );
            return None;
        }
    };
    if identity.validate().is_err() {
        report.finding(
            DiagnosticClass::Corrupt,
            relative,
            "invalid_identity_invariants",
        );
        return None;
    }
    Some(identity)
}

fn validate_identity_layout(
    layout: &ObservedLayout,
    identity: Option<&IdentityFileV1>,
    report: &mut LibraryInspectionReport,
) {
    let Some(identity) = identity else {
        return;
    };
    let relative = format!("{SYNC_DIR}/{IDENTITY_FILE}");
    let live_notebooks: BTreeSet<&str> = layout.notebooks.keys().map(String::as_str).collect();
    let identity_notebooks: BTreeSet<&str> =
        identity.notebooks.keys().map(String::as_str).collect();
    let pages_match = layout.notebooks.iter().all(|(notebook, pages)| {
        let expected: BTreeSet<&str> = pages.iter().map(String::as_str).collect();
        identity
            .pages
            .get(notebook)
            .map(|actual| actual.keys().map(String::as_str).collect::<BTreeSet<_>>() == expected)
            .unwrap_or(false)
    });
    if live_notebooks != identity_notebooks || !pages_match {
        report.finding(
            DiagnosticClass::Unverified,
            relative,
            "normal_sync_action=reconcile-identity",
        );
    } else {
        report.observe(DiagnosticClass::Valid);
    }
}

fn scan_mutation_generation(path: &Path, relative: &str, report: &mut LibraryInspectionReport) {
    let Some(bytes) = read_optional(path, relative, 64, report) else {
        return;
    };
    let canonical = bytes.last() == Some(&b'\n')
        && !bytes[..bytes.len().saturating_sub(1)].is_empty()
        && bytes[..bytes.len().saturating_sub(1)]
            .iter()
            .all(u8::is_ascii_digit)
        && (bytes.len() == 2 || bytes.first() != Some(&b'0'));
    if canonical
        && std::str::from_utf8(&bytes[..bytes.len() - 1])
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .is_some()
    {
        report.observe(DiagnosticClass::Valid);
    } else {
        report.finding(
            DiagnosticClass::Corrupt,
            relative.to_owned(),
            "invalid_mutation_generation",
        );
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationRecordV1Inspection {
    v: u32,
    notebook_id: String,
    objects: Vec<String>,
}

fn scan_migration_state(root: &Path, report: &mut LibraryInspectionReport) {
    let relative = MIGRATION_RECORD_FILE;
    if let Some(bytes) = read_optional(
        &root.join(MIGRATION_RECORD_FILE),
        relative,
        MAX_METADATA_BYTES,
        report,
    ) {
        report.finding(
            DiagnosticClass::IncompleteTransaction,
            relative,
            "library_migration_record_present",
        );
        if let Some(value) = strict_value(&bytes, relative, report) {
            let parsed: Result<MigrationRecordV1Inspection, _> = serde_json::from_value(value);
            let valid = parsed.is_ok_and(|record| {
                if record.v != 1 || record.notebook_id != "notebook-1" || record.objects.is_empty()
                {
                    return false;
                }
                let mut seen = HashSet::new();
                record.objects.iter().all(|object| {
                    let safe = object == MANIFEST_FILE
                        || object == PAGE_TRASH_DIR
                        || (canonical_ordinal_id(object, "page-") && !object.contains(['/', '\\']));
                    safe && seen.insert(object.as_str())
                }) && seen.contains(MANIFEST_FILE)
            });
            if !valid {
                report.finding(
                    DiagnosticClass::Corrupt,
                    relative,
                    "invalid_library_migration_record",
                );
            }
        }
    }
    if let Some(name) = root.file_name() {
        let mut sibling = name.to_os_string();
        sibling.push(".adopt");
        if root.with_file_name(sibling).exists() {
            report.finding(
                DiagnosticClass::IncompleteTransaction,
                ".",
                "library_adoption_staging_sibling_present",
            );
        }
    }
}

fn scan_root_entries(
    root: &Path,
    _manifest: Option<&PersistedLibraryManifestV1>,
    report: &mut LibraryInspectionReport,
) {
    const KNOWN: &[&str] = &[
        LIBRARY_MANIFEST_FILE,
        NOTEBOOKS_DIR,
        NOTEBOOK_TRASH_DIR,
        FOLDERS_DIR,
        IMPORT_DIR,
        SYNC_DIR,
        MIGRATION_RECORD_FILE,
    ];
    let Ok(read_dir) = fs::read_dir(root) else {
        return;
    };
    let mut names = read_dir
        .flatten()
        .filter_map(|entry| safe_component(&entry.file_name()).map(|name| (name, entry.path())))
        .collect::<Vec<_>>();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path) in names {
        if KNOWN.contains(&name.as_str()) {
            if name == NOTEBOOK_TRASH_DIR {
                if inspect_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                    report.finding(
                        DiagnosticClass::Unverified,
                        name,
                        "notebook_trash_preserved_not_semantically_validated",
                    );
                } else {
                    report.finding(
                        DiagnosticClass::InspectionError,
                        name,
                        "notebook_trash_not_directory",
                    );
                }
            }
            continue;
        }
        if is_temp_name(&name) {
            report.finding(
                DiagnosticClass::IncompleteTransaction,
                name,
                "temporary_library_object_present",
            );
        } else if name.starts_with(".scribe-dev-") {
            report.finding(
                DiagnosticClass::Unverified,
                name,
                "developer_trigger_present",
            );
        } else if name == MANIFEST_FILE || name.starts_with("page-") {
            report.finding(
                DiagnosticClass::IncompleteTransaction,
                name,
                "legacy_flat_library_layout_present",
            );
        } else if inspect_metadata(&path).is_ok() {
            report.finding(DiagnosticClass::Unverified, name, "unknown_library_object");
        }
    }
}

fn scan_aux_directory_entries(
    directory: &Path,
    relative_dir: &str,
    known: &[&str],
    report: &mut LibraryInspectionReport,
) {
    let Ok(read_dir) = fs::read_dir(directory) else {
        return;
    };
    let mut names = read_dir
        .flatten()
        .filter_map(|entry| safe_component(&entry.file_name()))
        .collect::<Vec<_>>();
    names.sort();
    for name in names {
        if known.contains(&name.as_str()) {
            continue;
        }
        let relative = format!("{relative_dir}/{name}");
        if is_temp_name(&name) {
            report.finding(
                DiagnosticClass::IncompleteTransaction,
                relative,
                "temporary_auxiliary_object_present",
            );
        } else {
            report.finding(
                DiagnosticClass::Unverified,
                relative,
                "unknown_auxiliary_object",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::RenderProfile;
    use crate::store::{
        BackgroundIdentityV1, PersistedPageHeader, PersistedPointV1, PersistedStrokeV1,
        STROKE_RECORD_VERSION,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str, versions: &[Vec<u32>]) -> Self {
            let root = std::env::temp_dir().join(format!(
                "scribe-library-inspect-{}-{}-{label}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join(NOTEBOOKS_DIR)).unwrap();
            let notebook_ids = (1..=versions.len())
                .map(|index| format!("notebook-{index}"))
                .collect::<Vec<_>>();
            write_json(
                &root.join(LIBRARY_MANIFEST_FILE),
                &PersistedLibraryManifestV1 {
                    v: 1,
                    notebook_ids: notebook_ids.clone(),
                    active_notebook_id: notebook_ids[0].clone(),
                    next_notebook_ordinal: notebook_ids.len() as u64 + 1,
                },
            );
            for (index, page_versions) in versions.iter().enumerate() {
                let notebook_id = &notebook_ids[index];
                let notebook = root.join(NOTEBOOKS_DIR).join(notebook_id);
                fs::create_dir_all(&notebook).unwrap();
                write_json(
                    &notebook.join(NOTEBOOK_INFO_FILE),
                    &PersistedNotebookInfoV1 {
                        v: 1,
                        title: format!("Fixture {}", index + 1),
                    },
                );
                let page_ids = (1..=page_versions.len())
                    .map(|page| format!("page-{page}"))
                    .collect::<Vec<_>>();
                write_json(
                    &notebook.join(MANIFEST_FILE),
                    &PersistedNotebookManifestV1 {
                        v: 1,
                        notebook_id: notebook_id.clone(),
                        page_ids: page_ids.clone(),
                        active_page_id: page_ids[0].clone(),
                        next_page_ordinal: page_ids.len() as u64 + 1,
                    },
                );
                for (page_id, version) in page_ids.iter().zip(page_versions) {
                    create_page(&notebook, page_id, *version);
                }
            }
            Self { root }
        }

        fn page(&self, notebook: usize, page: usize) -> PathBuf {
            self.root
                .join(NOTEBOOKS_DIR)
                .join(format!("notebook-{notebook}"))
                .join(format!("page-{page}"))
        }

        fn notebook(&self, notebook: usize) -> PathBuf {
            self.root
                .join(NOTEBOOKS_DIR)
                .join(format!("notebook-{notebook}"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = make_tree_writable(&self.root);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_json(path: &Path, value: &impl serde::Serialize) {
        let mut bytes = serde_json::to_vec_pretty(value).unwrap();
        bytes.push(b'\n');
        fs::write(path, bytes).unwrap();
    }

    fn create_page(notebook: &Path, page_id: &str, version: u32) {
        let page = notebook.join(page_id);
        fs::create_dir_all(&page).unwrap();
        let background = vec![0xff; 16];
        let (render, edit_journal_version) = match version {
            FORMAT_VERSION_V2 => (
                serde_json::to_value(
                    RenderProfile::sealed_v1(0)
                        .unwrap()
                        .to_persisted_v1()
                        .unwrap(),
                )
                .unwrap(),
                0,
            ),
            FORMAT_VERSION_V3 => (
                serde_json::to_value(
                    RenderProfile::sealed_v1(0)
                        .unwrap()
                        .to_persisted_v1()
                        .unwrap(),
                )
                .unwrap(),
                EDIT_JOURNAL_VERSION_V1,
            ),
            FORMAT_VERSION_V4 | FORMAT_VERSION => (
                serde_json::to_value(
                    RenderProfile::sealed_v2_pressure(0)
                        .unwrap()
                        .to_persisted_v2()
                        .unwrap(),
                )
                .unwrap(),
                if version == FORMAT_VERSION {
                    EDIT_JOURNAL_VERSION
                } else {
                    EDIT_JOURNAL_VERSION_V1
                },
            ),
            _ => panic!("unsupported fixture version"),
        };
        write_json(
            &page.join("page.meta"),
            &PersistedPageHeader {
                v: version,
                page_id: page_id.into(),
                canonical_width: notebook_core::CANONICAL_WIDTH,
                canonical_height: notebook_core::CANONICAL_HEIGHT,
                panel_width: 4,
                panel_height: 4,
                line_length: 4,
                background_bytes: background.len() as u64,
                background_checksum: rect_checksum(&background),
                bg_layout_version: BG_LAYOUT_VERSION,
                render,
                edit_journal_version,
                background_identity: Some(BackgroundIdentityV1::from_pixels(&background)),
            },
        );
        fs::write(page.join("background.raw"), background).unwrap();
        fs::write(page.join("strokes.jsonl"), b"").unwrap();
        if version != FORMAT_VERSION_V2 {
            fs::write(page.join("edits.jsonl"), b"").unwrap();
        }
    }

    fn write_identity(fixture: &Fixture) -> IdentityFileV1 {
        let library: PersistedLibraryManifestV1 =
            serde_json::from_slice(&fs::read(fixture.root.join(LIBRARY_MANIFEST_FILE)).unwrap())
                .unwrap();
        let mut live = sync_protocol::LiveLayout::default();
        for notebook_id in library.notebook_ids {
            let manifest: PersistedNotebookManifestV1 = serde_json::from_slice(
                &fs::read(
                    fixture
                        .root
                        .join(NOTEBOOKS_DIR)
                        .join(&notebook_id)
                        .join(MANIFEST_FILE),
                )
                .unwrap(),
            )
            .unwrap();
            live.notebooks.push((notebook_id, manifest.page_ids));
        }
        let identity = sync_protocol::reconcile_identity(None, &live, &mut uuid::Uuid::now_v7)
            .unwrap()
            .identity;
        fs::create_dir_all(fixture.root.join(SYNC_DIR)).unwrap();
        let mut bytes = identity.to_canonical_bytes().unwrap();
        bytes.push(b'\n');
        fs::write(fixture.root.join(SYNC_DIR).join(IDENTITY_FILE), bytes).unwrap();
        identity
    }

    fn tree_manifest(root: &Path) -> BTreeMap<String, (String, u64, String)> {
        fn walk(root: &Path, path: &Path, out: &mut BTreeMap<String, (String, u64, String)>) {
            let mut entries = fs::read_dir(path)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let metadata = fs::symlink_metadata(&path).unwrap();
                if metadata.is_dir() {
                    out.insert(relative, ("directory".into(), 0, "-".into()));
                    walk(root, &path, out);
                } else if metadata.is_file() {
                    let bytes = fs::read(&path).unwrap();
                    out.insert(
                        relative,
                        (
                            "file".into(),
                            bytes.len() as u64,
                            format!("{:x}", Sha256::digest(&bytes)),
                        ),
                    );
                } else {
                    out.insert(relative, ("other".into(), 0, "-".into()));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    #[allow(clippy::permissions_set_readonly_false)]
    fn make_tree_writable(root: &Path) -> std::io::Result<()> {
        if !root.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = fs::metadata(&path)?.permissions();
                permissions.set_mode(permissions.mode() | 0o700);
                fs::set_permissions(&path, permissions)?;
            }
            #[cfg(not(unix))]
            {
                // Windows exposes only the read-only bit through std; clearing it is required
                // to let TempDir remove the disposable fixture.
                let mut permissions = fs::metadata(&path)?.permissions();
                permissions.set_readonly(false);
                fs::set_permissions(&path, permissions)?;
            }
            if path.is_dir() {
                make_tree_writable(&path)?;
            }
        }
        Ok(())
    }

    #[test]
    fn valid_historical_multi_notebook_scan_is_deterministic_and_non_mutating() {
        let fixture = Fixture::new("valid-history", &[vec![2, 3], vec![4, 5]]);
        let before = tree_manifest(&fixture.root);
        let first = inspect_library(&fixture.root);
        let first_bytes = first.canonical_bytes().unwrap();
        let second = inspect_library(&fixture.root);
        let after = tree_manifest(&fixture.root);

        assert_eq!(
            first.result_token(),
            "VALID",
            "{}",
            String::from_utf8_lossy(&first_bytes)
        );
        assert_eq!(first.counts.notebooks, 2);
        assert_eq!(first.counts.pages, 4);
        assert_eq!(first.observed_page_formats, BTreeSet::from([2, 3, 4, 5]));
        assert_eq!(first_bytes, second.canonical_bytes().unwrap());
        assert_eq!(before, after);
        assert!(first_bytes
            .windows(b"data_compatibility=unknown".len())
            .any(|w| { w == b"data_compatibility=unknown" }));
    }

    #[test]
    fn validates_journals_optional_content_and_auxiliary_state() {
        let fixture = Fixture::new("content", &[vec![5, 5]]);
        let identity = write_identity(&fixture);
        let page = fixture.page(1, 1);
        let stroke = PersistedStrokeV1 {
            v: STROKE_RECORD_VERSION,
            record_type: "stroke".into(),
            id: "stroke-1".into(),
            finished: true,
            points: vec![PersistedPointV1 {
                x: 10.0,
                y: 20.0,
                pressure: None,
            }],
        };
        fs::write(
            page.join("strokes.jsonl"),
            format!("{}\n", serde_json::to_string(&stroke).unwrap()),
        )
        .unwrap();
        fs::write(
            page.join("edits.jsonl"),
            b"{\"v\":1,\"type\":\"delete_stroke\",\"stroke_id\":\"stroke-1\"}\n",
        )
        .unwrap();
        let text_id = uuid::Uuid::now_v7();
        fs::write(
            page.join("texts.jsonl"),
            format!(
                "{{\"text_id\":\"{text_id}\",\"x\":1.0,\"y\":2.0,\"width\":100.0,\"font_size\":12.0,\"face\":\"default-sans-v1\",\"content\":\"fixture\"}}\n"
            ),
        )
        .unwrap();
        let notebook_global = identity.notebook_id("notebook-1").unwrap();
        let page_global = identity.page_id("notebook-1", "page-1").unwrap();
        let target_notebook = notebook_global;
        let target_page = identity.page_id("notebook-1", "page-2").unwrap();
        let link_id = uuid::Uuid::now_v7();
        fs::write(
            page.join("links.jsonl"),
            format!(
                "{{\"link_id\":\"{link_id}\",\"source_notebook_id\":\"{notebook_global}\",\"source_page_id\":\"{page_global}\",\"source_x\":1.0,\"source_y\":2.0,\"target_notebook_id\":\"{target_notebook}\",\"target_page_id\":\"{target_page}\"}}\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(fixture.root.join(FOLDERS_DIR)).unwrap();
        fs::write(
            fixture.root.join(FOLDERS_DIR).join(FOLDERS_FILE),
            b"{\"v\":1,\"next_folder_ordinal\":2,\"folders\":[{\"id\":\"folder-1\",\"title\":\"Folder\",\"notebook_ids\":[\"notebook-1\"]}]}\n",
        )
        .unwrap();
        let origin = uuid::Uuid::now_v7().to_string();
        fs::create_dir_all(fixture.root.join(IMPORT_DIR)).unwrap();
        fs::write(
            fixture.root.join(IMPORT_DIR).join(RECEIPTS_FILE),
            format!(
                "{{\"v\":1,\"receipts\":{{\"amazon_nbk:{origin}\":{{\"origin_kind\":\"amazon_nbk\",\"origin_id\":\"{origin}\",\"state\":\"complete\",\"dest_notebook_id\":\"notebook-1\",\"title\":\"Fixture\",\"book_id\":null,\"started_unix_ms\":1,\"finished_unix_ms\":2,\"pages\":2,\"strokes\":1,\"error\":null}}}}}}\n"
            ),
        )
        .unwrap();
        fs::write(
            fixture.notebook(1).join(ORIGIN_MARKER),
            format!(
                "{{\"v\":1,\"origin_kind\":\"amazon_nbk\",\"origin_id\":\"{origin}\",\"imported_unix_ms\":2}}\n"
            ),
        )
        .unwrap();

        let report = inspect_library(&fixture.root);
        assert_eq!(report.result_token(), "VALID", "{:?}", report.findings());
        assert_eq!(report.counts.stroke_records, 1);
        assert_eq!(report.counts.edit_records, 1);
        assert_eq!(report.counts.text_records, 1);
        assert_eq!(report.counts.link_records, 1);
        assert_eq!(report.counts.folder_records, 1);
        assert_eq!(report.counts.import_receipts, 1);
    }

    #[test]
    fn missing_duplicate_unsupported_and_unknown_variants_fail_closed() {
        let fixture = Fixture::new("invalid-structure", &[vec![5, 5]]);
        fs::remove_dir_all(fixture.page(1, 2)).unwrap();
        let page = fixture.page(1, 1);
        let mut header: Value =
            serde_json::from_slice(&fs::read(page.join("page.meta")).unwrap()).unwrap();
        header["v"] = Value::from(99);
        write_json(&page.join("page.meta"), &header);
        fs::write(
            page.join("strokes.jsonl"),
            b"{\"v\":9,\"type\":\"future\"}\n",
        )
        .unwrap();
        let mut library = fs::read_to_string(fixture.root.join(LIBRARY_MANIFEST_FILE)).unwrap();
        library = library.replacen("\"v\": 1", "\"v\": 1,\n  \"v\": 1", 1);
        fs::write(fixture.root.join(LIBRARY_MANIFEST_FILE), library).unwrap();

        let report = inspect_library(&fixture.root);
        assert_eq!(report.result_token(), "ISSUES_DETECTED");
        assert!(report.diagnostic_count(DiagnosticClass::Corrupt) > 0);
        assert!(report
            .findings()
            .iter()
            .any(|finding| finding.reason == "duplicate_json_key"));

        let notebook_manifest = fixture.notebook(1).join(MANIFEST_FILE);
        write_json(
            &fixture.root.join(LIBRARY_MANIFEST_FILE),
            &PersistedLibraryManifestV1 {
                v: 1,
                notebook_ids: vec!["notebook-1".into()],
                active_notebook_id: "notebook-1".into(),
                next_notebook_ordinal: 2,
            },
        );
        write_json(
            &notebook_manifest,
            &PersistedNotebookManifestV1 {
                v: 1,
                notebook_id: "notebook-1".into(),
                page_ids: vec!["page-1".into(), "page-1".into()],
                active_page_id: "page-1".into(),
                next_page_ordinal: 2,
            },
        );
        let report = inspect_library(&fixture.root);
        assert!(report
            .findings()
            .iter()
            .any(|finding| { finding.reason == "invalid_notebook_manifest_invariants" }));

        // Restore the library manifest so referenced-object failures remain independently seen.
        write_json(
            &fixture.root.join(LIBRARY_MANIFEST_FILE),
            &PersistedLibraryManifestV1 {
                v: 1,
                notebook_ids: vec!["notebook-1".into()],
                active_notebook_id: "notebook-1".into(),
                next_notebook_ordinal: 2,
            },
        );
        write_json(
            &notebook_manifest,
            &PersistedNotebookManifestV1 {
                v: 1,
                notebook_id: "notebook-1".into(),
                page_ids: vec!["page-1".into(), "page-2".into()],
                active_page_id: "page-1".into(),
                next_page_ordinal: 3,
            },
        );
        let report = inspect_library(&fixture.root);
        assert!(report.diagnostic_count(DiagnosticClass::MissingRequired) > 0);
        assert!(report.diagnostic_count(DiagnosticClass::UnsupportedFormat) > 0);
    }

    #[test]
    fn torn_journals_markers_trash_and_orphans_are_preserved_and_reported() {
        let fixture = Fixture::new("interrupted", &[vec![5]]);
        let page = fixture.page(1, 1);
        fs::write(
            page.join("strokes.jsonl"),
            b"{\"v\":1,\"type\":\"stroke\",\"id\":\"s\",\"finished\":true,\"points\":[{\"x\":1.0,\"y\":1.0}]}\npartial",
        )
        .unwrap();
        fs::write(page.join("edits.jsonl"), b"partial").unwrap();
        fs::create_dir_all(fixture.root.join(NOTEBOOK_TRASH_DIR).join("saved")).unwrap();
        fs::create_dir_all(fixture.notebook(1).join(PAGE_TRASH_DIR).join("saved")).unwrap();
        fs::create_dir_all(fixture.root.join(NOTEBOOKS_DIR).join("notebook-9")).unwrap();
        fs::create_dir_all(fixture.notebook(1).join("page-9")).unwrap();
        fs::write(
            fixture.root.join(MIGRATION_RECORD_FILE),
            b"{\"v\":1,\"notebook_id\":\"notebook-1\",\"objects\":[\"notebook.meta\",\"page-1\"]}\n",
        )
        .unwrap();
        let before = tree_manifest(&fixture.root);
        let report = inspect_library(&fixture.root);
        assert_eq!(before, tree_manifest(&fixture.root));
        assert!(report.diagnostic_count(DiagnosticClass::IncompleteTransaction) >= 3);
        assert!(report.diagnostic_count(DiagnosticClass::Unverified) >= 4);
    }

    #[test]
    fn corrupt_page_does_not_hide_later_required_coverage() {
        let fixture = Fixture::new("continue", &[vec![5, 5]]);
        fs::write(fixture.page(1, 1).join("strokes.jsonl"), b"not-json\n").unwrap();
        let text_id = uuid::Uuid::now_v7();
        fs::write(
            fixture.page(1, 2).join("texts.jsonl"),
            format!(
                "{{\"text_id\":\"{text_id}\",\"x\":1.0,\"y\":2.0,\"width\":100.0,\"font_size\":12.0,\"face\":\"default-sans-v1\",\"content\":\"still inspected\"}}\n"
            ),
        )
        .unwrap();
        let report = inspect_library(&fixture.root);
        assert!(report.diagnostic_count(DiagnosticClass::Corrupt) > 0);
        assert_eq!(report.counts.text_records, 1);
        assert_eq!(report.counts.pages, 2);
    }

    #[test]
    fn duplicate_keys_deep_json_oversized_records_and_finding_limits_fail_closed() {
        let fixture = Fixture::new("limits", &[vec![5]]);
        let page = fixture.page(1, 1);
        fs::write(
            page.join("strokes.jsonl"),
            vec![b'x'; MAX_RECORD_BYTES as usize + 2],
        )
        .unwrap();
        let mut nested = String::new();
        for _ in 0..=MAX_JSON_DEPTH {
            nested.push_str("{\"x\":");
        }
        nested.push('0');
        for _ in 0..=MAX_JSON_DEPTH {
            nested.push('}');
        }
        fs::write(page.join("texts.jsonl"), format!("{nested}\n")).unwrap();
        for index in 0..=MAX_FINDINGS {
            fs::write(fixture.root.join(format!("unknown-{index}")), b"x").unwrap();
        }
        let report = inspect_library(&fixture.root);
        assert!(!report.completed);
        assert!(report
            .findings()
            .iter()
            .any(|finding| finding.reason == "journal_record_size_limit_exceeded"));
        assert!(report
            .findings()
            .iter()
            .any(|finding| finding.reason == "finding_limit_exceeded"));
        assert!(report.canonical_bytes().unwrap().len() <= REPORT_MAX_BYTES);
    }

    #[test]
    fn missing_and_empty_library_are_bounded_failures() {
        let missing = std::env::temp_dir().join(format!(
            "scribe-library-inspect-missing-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&missing);
        let report = inspect_library(&missing);
        assert!(!report.completed);
        assert_eq!(report.counts.libraries, 0);

        fs::create_dir_all(&missing).unwrap();
        let report = inspect_library(&missing);
        assert!(report.diagnostic_count(DiagnosticClass::MissingRequired) >= 2);
        fs::remove_dir_all(&missing).unwrap();
    }

    #[test]
    fn read_only_fixture_is_not_modified() {
        let fixture = Fixture::new("readonly", &[vec![5]]);
        let before = tree_manifest(&fixture.root);
        for path in before.keys() {
            let absolute = fixture
                .root
                .join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            if absolute.is_file() {
                let mut permissions = fs::metadata(&absolute).unwrap().permissions();
                permissions.set_readonly(true);
                fs::set_permissions(&absolute, permissions).unwrap();
            }
        }
        let report = inspect_library(&fixture.root);
        assert_eq!(report.result_token(), "VALID", "{:?}", report.findings());
        assert_eq!(before, tree_manifest(&fixture.root));
    }

    #[test]
    fn a_change_between_scan_and_final_census_is_reported_as_uncertain() {
        let fixture = Fixture::new("concurrent", &[vec![5]]);
        let changed = fixture.page(1, 1).join("strokes.jsonl");
        let report = inspect_library_with_hook(&fixture.root, || {
            fs::write(&changed, b"partial").unwrap();
        });
        assert!(!report.completed);
        assert!(report
            .findings()
            .iter()
            .any(|finding| { finding.reason == "filesystem_changed_during_inspection" }));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_special_files_are_refused_without_following() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("unsafe", &[vec![5]]);
        let outside = fixture.root.with_extension("outside");
        fs::write(&outside, b"secret").unwrap();
        let root_alias = fixture.root.with_extension("root-link");
        symlink(&fixture.root, &root_alias).unwrap();
        let alias_report = inspect_library(&root_alias);
        assert!(alias_report
            .findings()
            .iter()
            .any(|finding| { finding.reason == "library_root_path_contains_symlink" }));
        fs::remove_file(&root_alias).unwrap();
        symlink(&outside, fixture.root.join("escape")).unwrap();
        fs::hard_link(
            fixture.root.join(LIBRARY_MANIFEST_FILE),
            fixture.root.join("hard-link"),
        )
        .unwrap();
        let fifo = fixture.root.join("fifo");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let report = inspect_library(&fixture.root);
        assert!(!report.completed);
        assert!(report
            .findings()
            .iter()
            .any(|finding| finding.reason == "symlink_refused"));
        assert!(report
            .findings()
            .iter()
            .any(|finding| finding.reason == "unsupported_special_file"));
        assert!(report
            .findings()
            .iter()
            .any(|finding| finding.reason == "hard_link_refused"));
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        fs::remove_file(outside).unwrap();
    }
}
