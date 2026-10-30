//! Bounded, read-only fixed-root inventory and compatibility preflight.
//!
//! The preflight deliberately provides observational evidence only. It does not acquire a writer
//! boundary, run retained/candidate binaries, open storage through migrating APIs, or authorize a
//! launch. Exact A/B provenance and bidirectional compatibility remain unavailable.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

pub const REPORT_SCHEMA: u32 = 1;
pub const INVENTORY_SCHEMA: u32 = 1;
pub const REPORT_MAX_BYTES: usize = 512 * 1024;
pub const MAX_FINDINGS: usize = 512;
pub const MAX_TREE_ENTRIES: usize = 200_000;
pub const MAX_TREE_DEPTH: usize = 8;
pub const MAX_RELATIVE_PATH_BYTES: usize = 512;
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreflightConfig {
    pub app_root: PathBuf,
    pub library_root: PathBuf,
    pub sync_root: PathBuf,
    pub device_root: PathBuf,
    pub state_root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectionCommand {
    NotRequested,
    Inspect(PreflightConfig),
}

pub fn parse_command(args: &[String]) -> Result<InspectionCommand, String> {
    let requested = args
        .iter()
        .filter(|arg| arg.as_str() == "--inspect-compatibility-preflight")
        .count();
    if requested == 0 {
        return Ok(InspectionCommand::NotRequested);
    }
    let expected = [
        "--inspect-compatibility-preflight",
        "--app-root",
        "",
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
        || [2usize, 4, 6, 8, 10]
            .iter()
            .any(|index| args[*index].is_empty())
    {
        return Err("--inspect-compatibility-preflight must be used exactly as: --inspect-compatibility-preflight --app-root APP --data-dir LIBRARY --sync-dir SYNC --device-dir DEVICE --state-dir STATE".into());
    }
    let config = PreflightConfig {
        app_root: PathBuf::from(&args[2]),
        library_root: PathBuf::from(&args[4]),
        sync_root: PathBuf::from(&args[6]),
        device_root: PathBuf::from(&args[8]),
        state_root: PathBuf::from(&args[10]),
    };
    validate_config(&config)?;
    Ok(InspectionCommand::Inspect(config))
}

fn absolute_clean(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| !matches!(component, Component::CurDir | Component::ParentDir))
}

fn validate_config(config: &PreflightConfig) -> Result<(), String> {
    let paths = [
        &config.app_root,
        &config.library_root,
        &config.sync_root,
        &config.device_root,
        &config.state_root,
    ];
    if paths.iter().any(|path| !absolute_clean(path)) {
        return Err("compatibility-preflight paths must be absolute and traversal-free".into());
    }
    let unique: BTreeSet<_> = paths.iter().map(|path| path.as_path()).collect();
    if unique.len() != paths.len() {
        return Err("compatibility-preflight paths must be distinct".into());
    }
    if config.state_root != config.app_root.join("state")
        || config.sync_root != config.state_root.join("sync")
        || config.device_root != config.state_root.join("device")
    {
        return Err(
            "compatibility-preflight APP/STATE/SYNC/DEVICE paths do not match the fixed production layout"
                .into(),
        );
    }
    if config.library_root.starts_with(&config.app_root)
        || config.app_root.starts_with(&config.library_root)
    {
        return Err(
            "compatibility-preflight library and application roots must not overlap".into(),
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FindingClass {
    Missing,
    Unsafe,
    Changed,
    Limit,
    Interrupted,
    Io,
    Unexpected,
}

impl FindingClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unsafe => "unsafe",
            Self::Changed => "changed",
            Self::Limit => "limit",
            Self::Interrupted => "interrupted",
            Self::Io => "io",
            Self::Unexpected => "unexpected",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct InventoryFinding {
    class: FindingClass,
    role: String,
    relative_path: String,
    reason: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EntryKind {
    Directory,
    File,
    Absent,
    Delegated,
}

impl EntryKind {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
            Self::Absent => "absent",
            Self::Delegated => "delegated",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InventoryEntry {
    kind: EntryKind,
    bytes: u64,
    sha256: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct InventoryLimits {
    entries: usize,
    depth: usize,
    file_bytes: u64,
    total_bytes: u64,
}

impl Default for InventoryLimits {
    fn default() -> Self {
        Self {
            entries: MAX_TREE_ENTRIES,
            depth: MAX_TREE_DEPTH,
            file_bytes: MAX_FILE_BYTES,
            total_bytes: MAX_TOTAL_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventorySnapshot {
    entries: BTreeMap<(String, String), InventoryEntry>,
    findings: Vec<InventoryFinding>,
    findings_overflowed: bool,
    files: u64,
    directories: u64,
    bytes_hashed: u64,
}

impl InventorySnapshot {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            findings: Vec::new(),
            findings_overflowed: false,
            files: 0,
            directories: 0,
            bytes_hashed: 0,
        }
    }

    pub fn complete(&self) -> bool {
        self.findings.is_empty() && !self.findings_overflowed
    }

    fn finding(
        &mut self,
        class: FindingClass,
        role: impl Into<String>,
        relative_path: impl Into<String>,
        reason: &'static str,
    ) {
        if self.findings.len() < MAX_FINDINGS {
            self.findings.push(InventoryFinding {
                class,
                role: role.into(),
                relative_path: relative_path.into(),
                reason,
            });
        } else {
            self.findings_overflowed = true;
        }
    }

    fn finish(&mut self) {
        self.findings.sort();
        self.findings.dedup();
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, String> {
        let mut output = String::new();
        output.push_str("IGNITE_FIXED_ROOT_INVENTORY_V1\n");
        output.push_str(&format!("schema={INVENTORY_SCHEMA}\n"));
        output.push_str("manifest=kindle-persistent-state-v1\n");
        output.push_str(&format!("complete={}\n", self.complete()));
        output.push_str(&format!("entry_count={}\n", self.entries.len()));
        output.push_str(&format!("files={}\n", self.files));
        output.push_str(&format!("directories={}\n", self.directories));
        output.push_str(&format!("bytes_hashed={}\n", self.bytes_hashed));
        output.push_str(&format!("finding_count={}\n", self.findings.len()));
        output.push_str(&format!(
            "findings_overflowed={}\n",
            self.findings_overflowed
        ));
        for ((role, relative), entry) in &self.entries {
            if !safe_field(role) || !safe_field(relative) {
                return Err("unsafe inventory entry field".into());
            }
            output.push_str(&format!(
                "entry={role}|{relative}|{}|{}|{}\n",
                entry.kind.as_str(),
                entry.bytes,
                entry.sha256.as_deref().unwrap_or("-")
            ));
        }
        for finding in &self.findings {
            if !safe_field(&finding.role)
                || !safe_field(&finding.relative_path)
                || !safe_field(finding.reason)
            {
                return Err("unsafe inventory finding field".into());
            }
            output.push_str(&format!(
                "finding={}|{}|{}|{}\n",
                finding.class.as_str(),
                finding.role,
                finding.relative_path,
                finding.reason
            ));
        }
        Ok(output.into_bytes())
    }

    pub fn digest(&self) -> Result<String, String> {
        Ok(format!("{:x}", Sha256::digest(self.canonical_bytes()?)))
    }

    fn has_class(&self, class: FindingClass) -> bool {
        self.findings.iter().any(|finding| finding.class == class)
    }
}

fn safe_field(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RELATIVE_PATH_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/' | b'_' | b'-' | b'=' | b':')
        })
}

#[derive(Clone, Debug)]
struct RootSpec {
    role: String,
    path: PathBuf,
    required: bool,
    exclusions: BTreeMap<PathBuf, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InventoryEvent<'a> {
    FileOpened(&'a Path),
    DirectoryEnumerated(&'a Path),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MetadataSignature {
    kind: &'static str,
    bytes: u64,
    identity: FileIdentity,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    links: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(windows)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    attributes: u32,
    creation_time: u64,
    write_time: u64,
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    modified: Option<std::time::SystemTime>,
}

fn metadata_signature(metadata: &fs::Metadata) -> Result<MetadataSignature, &'static str> {
    let kind = if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else {
        "special"
    };
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            links: metadata.nlink(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    };
    #[cfg(windows)]
    let identity = {
        use std::os::windows::fs::MetadataExt;
        FileIdentity {
            attributes: metadata.file_attributes(),
            creation_time: metadata.creation_time(),
            write_time: metadata.last_write_time(),
        }
    };
    #[cfg(not(any(unix, windows)))]
    let identity = FileIdentity {
        modified: metadata.modified().ok(),
    };
    Ok(MetadataSignature {
        kind,
        bytes: metadata.len(),
        identity,
    })
}

fn singly_linked(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() == 1
    }
    #[cfg(windows)]
    {
        // Stable Rust does not expose a link count in `MetadataExt` on Windows. Kindle's Unix
        // implementation performs the authoritative single-link check; Windows is compile and
        // disposable-fixture coverage only.
        let _ = metadata;
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        false
    }
}

fn path_chain_contains_symlink(path: &Path) -> bool {
    crate::storage_inspect_library::path_chain_contains_symlink(path)
}

fn observed_directory(path: &Path) -> Result<Vec<(String, MetadataSignature)>, &'static str> {
    let mut entries = fs::read_dir(path)
        .map_err(|_| "directory_read_failed")?
        .map(|entry| entry.map_err(|_| "directory_entry_failed"))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut observed = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = crate::storage_inspect_library::safe_component(&entry.file_name())
            .ok_or("unsafe_entry_name")?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|_| "metadata_unavailable")?;
        observed.push((name, metadata_signature(&metadata)?));
    }
    Ok(observed)
}

fn insert_entry(
    snapshot: &mut InventorySnapshot,
    limits: InventoryLimits,
    role: &str,
    relative: &str,
    entry: InventoryEntry,
) -> bool {
    if snapshot.entries.len() >= limits.entries {
        snapshot.finding(
            FindingClass::Limit,
            role,
            relative,
            "entry_count_limit_exceeded",
        );
        return false;
    }
    snapshot
        .entries
        .insert((role.to_owned(), relative.to_owned()), entry);
    true
}

fn known_top_level(role: &str, name: &str) -> bool {
    if role == "library" || role == "library_adopt" || role.starts_with("retired_library:") {
        return matches!(
            name,
            "library.meta"
                | "notebooks"
                | ".scribe-folders"
                | ".scribe-import"
                | ".notebook-trash"
                | ".scribe-sync"
                | ".scribe-library-migration-v1"
                | ".notebook-1.migrating"
        );
    }
    if role == "sync" || role.starts_with("retired_sync:") {
        return matches!(
            name,
            "config.json"
                | "sync-state.json"
                | "genesis.json"
                | "base-state.json"
                | "outbox.jsonl"
                | "blobs"
                | "sync.lock"
                | "replica.json"
                | "replica.lock"
                | "accepted.jsonl"
                | "inbox.jsonl"
                | "apply.json"
                | "confirmed.log"
                | "checkpoint"
                | "local-verified-v1.json"
                | "local-verified-v1.json.cleared"
                | "local-verified-baseline-v1.json"
                | "recovery-intent-v1.json"
                | "recovery-complete-v1.json"
                | "preferences.json"
                | "recovery-intent-v1.claim"
        );
    }
    if role == "sync_adopt" {
        return matches!(name, "plan.json" | "sync" | "server-profile.json");
    }
    if role == "device" {
        return matches!(
            name,
            "device.json" | "server-profile.json" | "preferences.json"
        );
    }
    if role == "state" {
        return matches!(
            name,
            "sync"
                | "device"
                | "update"
                | "preferences.json"
                | "replica.json"
                | "replica.lock"
                | "sync.adopt"
                | "sync.adopt.lock"
        ) || name.starts_with("sync.retired-");
    }
    if role == "activation_run" {
        return matches!(
            name,
            "ignite-activate.journal"
                | "ignite-activate.journal.tmp"
                | "ignite-activate.committed"
                | "ignite-activate.committed.tmp"
        );
    }
    false
}

fn hash_regular_file(
    path: &Path,
    expected: &fs::Metadata,
    role: &str,
    relative: &str,
    snapshot: &mut InventorySnapshot,
    limits: InventoryLimits,
    hook: &mut impl FnMut(InventoryEvent<'_>) -> bool,
) -> Option<InventoryEntry> {
    if path_chain_contains_symlink(path) {
        snapshot.finding(
            FindingClass::Unsafe,
            role,
            relative,
            "file_path_contains_symlink",
        );
        return None;
    }
    if expected.len() > limits.file_bytes {
        snapshot.finding(
            FindingClass::Limit,
            role,
            relative,
            "file_size_limit_exceeded",
        );
        return None;
    }
    if snapshot.bytes_hashed.saturating_add(expected.len()) > limits.total_bytes {
        snapshot.finding(
            FindingClass::Limit,
            role,
            relative,
            "total_byte_limit_exceeded",
        );
        return None;
    }
    if !singly_linked(expected) {
        snapshot.finding(FindingClass::Unsafe, role, relative, "hard_link_refused");
        return None;
    }
    let mut file = match crate::storage_inspect_library::open_regular_read_only(path, expected) {
        Ok(file) => file,
        Err(reason) => {
            snapshot.finding(FindingClass::Unsafe, role, relative, reason);
            return None;
        }
    };
    if !hook(InventoryEvent::FileOpened(path)) {
        snapshot.finding(
            FindingClass::Interrupted,
            role,
            relative,
            "inventory_interrupted",
        );
        return None;
    }
    let before = match file
        .metadata()
        .and_then(|metadata| metadata_signature(&metadata).map_err(std::io::Error::other))
    {
        Ok(signature) => signature,
        Err(_) => {
            snapshot.finding(
                FindingClass::Io,
                role,
                relative,
                "descriptor_metadata_failed",
            );
            return None;
        }
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        let count = match file.read(&mut buffer) {
            Ok(count) => count,
            Err(_) => {
                snapshot.finding(FindingClass::Io, role, relative, "file_read_failed");
                return None;
            }
        };
        if count == 0 {
            break;
        }
        read = read.saturating_add(count as u64);
        if read > limits.file_bytes {
            snapshot.finding(
                FindingClass::Limit,
                role,
                relative,
                "file_size_limit_exceeded",
            );
            return None;
        }
        hasher.update(&buffer[..count]);
    }
    let descriptor_after = match file
        .metadata()
        .and_then(|metadata| metadata_signature(&metadata).map_err(std::io::Error::other))
    {
        Ok(signature) => signature,
        Err(_) => {
            snapshot.finding(
                FindingClass::Io,
                role,
                relative,
                "descriptor_metadata_failed",
            );
            return None;
        }
    };
    let pathname_after = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            snapshot.finding(
                FindingClass::Changed,
                role,
                relative,
                "file_path_changed_during_read",
            );
            return None;
        }
    };
    let pathname_signature = match metadata_signature(&pathname_after) {
        Ok(signature) => signature,
        Err(reason) => {
            snapshot.finding(FindingClass::Io, role, relative, reason);
            return None;
        }
    };
    if path_chain_contains_symlink(path)
        || read != expected.len()
        || before != descriptor_after
        || descriptor_after != pathname_signature
        || !pathname_after.is_file()
        || !singly_linked(&pathname_after)
    {
        snapshot.finding(
            FindingClass::Changed,
            role,
            relative,
            "file_changed_during_read",
        );
        return None;
    }
    snapshot.files = snapshot.files.saturating_add(1);
    snapshot.bytes_hashed = snapshot.bytes_hashed.saturating_add(read);
    Some(InventoryEntry {
        kind: EntryKind::File,
        bytes: read,
        sha256: Some(format!("{:x}", hasher.finalize())),
    })
}

#[allow(clippy::too_many_arguments)]
fn scan_directory(
    path: &Path,
    role: &str,
    relative: &str,
    depth: usize,
    exclusions: &BTreeMap<PathBuf, String>,
    snapshot: &mut InventorySnapshot,
    limits: InventoryLimits,
    hook: &mut impl FnMut(InventoryEvent<'_>) -> bool,
) {
    if depth > limits.depth {
        snapshot.finding(
            FindingClass::Limit,
            role,
            relative,
            "tree_depth_limit_exceeded",
        );
        return;
    }
    if path_chain_contains_symlink(path) {
        snapshot.finding(
            FindingClass::Unsafe,
            role,
            relative,
            "directory_path_contains_symlink",
        );
        return;
    }
    let directory_before = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            match metadata_signature(&metadata) {
                Ok(signature) => signature,
                Err(reason) => {
                    snapshot.finding(FindingClass::Io, role, relative, reason);
                    return;
                }
            }
        }
        Ok(_) => {
            snapshot.finding(
                FindingClass::Unsafe,
                role,
                relative,
                "root_or_directory_not_directory",
            );
            return;
        }
        Err(_) => {
            snapshot.finding(
                FindingClass::Changed,
                role,
                relative,
                "directory_disappeared",
            );
            return;
        }
    };
    if !insert_entry(
        snapshot,
        limits,
        role,
        relative,
        InventoryEntry {
            kind: EntryKind::Directory,
            bytes: 0,
            sha256: None,
        },
    ) {
        return;
    }
    snapshot.directories = snapshot.directories.saturating_add(1);
    let observed_before = match observed_directory(path) {
        Ok(entries) => entries,
        Err(reason) => {
            snapshot.finding(FindingClass::Unsafe, role, relative, reason);
            return;
        }
    };
    if !hook(InventoryEvent::DirectoryEnumerated(path)) {
        snapshot.finding(
            FindingClass::Interrupted,
            role,
            relative,
            "inventory_interrupted",
        );
        return;
    }
    for (name, signature) in &observed_before {
        if path_chain_contains_symlink(path)
            || fs::symlink_metadata(path)
                .ok()
                .and_then(|metadata| metadata_signature(&metadata).ok())
                .as_ref()
                != Some(&directory_before)
        {
            snapshot.finding(
                FindingClass::Changed,
                role,
                relative,
                "directory_changed_before_entry_open",
            );
            return;
        }
        let child = path.join(name);
        let Some(child_relative) = crate::storage_inspect_library::join_rel(relative, name) else {
            snapshot.finding(
                FindingClass::Limit,
                role,
                relative,
                "relative_path_limit_exceeded",
            );
            continue;
        };
        if relative == "." && !known_top_level(role, name) {
            snapshot.finding(
                FindingClass::Unexpected,
                role,
                &child_relative,
                "unknown_top_level_entry_preserved",
            );
        }
        if let Some(delegated_role) = exclusions.get(&child) {
            insert_entry(
                snapshot,
                limits,
                role,
                &child_relative,
                InventoryEntry {
                    kind: EntryKind::Delegated,
                    bytes: 0,
                    sha256: Some(format!("role:{delegated_role}")),
                },
            );
            continue;
        }
        let metadata = match fs::symlink_metadata(&child) {
            Ok(metadata) => metadata,
            Err(_) => {
                snapshot.finding(
                    FindingClass::Changed,
                    role,
                    &child_relative,
                    "entry_disappeared",
                );
                continue;
            }
        };
        if metadata_signature(&metadata).as_ref() != Ok(signature) {
            snapshot.finding(
                FindingClass::Changed,
                role,
                &child_relative,
                "entry_changed_after_enumeration",
            );
            continue;
        }
        if metadata.file_type().is_symlink() {
            snapshot.finding(
                FindingClass::Unsafe,
                role,
                &child_relative,
                "symlink_refused",
            );
        } else if metadata.is_dir() {
            scan_directory(
                &child,
                role,
                &child_relative,
                depth + 1,
                exclusions,
                snapshot,
                limits,
                hook,
            );
        } else if metadata.is_file() {
            if let Some(entry) = hash_regular_file(
                &child,
                &metadata,
                role,
                &child_relative,
                snapshot,
                limits,
                hook,
            ) {
                insert_entry(snapshot, limits, role, &child_relative, entry);
            }
        } else {
            snapshot.finding(
                FindingClass::Unsafe,
                role,
                &child_relative,
                "special_file_refused",
            );
        }
    }
    let observed_after = observed_directory(path);
    let directory_after = fs::symlink_metadata(path)
        .ok()
        .and_then(|metadata| metadata_signature(&metadata).ok());
    if observed_after.as_ref() != Ok(&observed_before)
        || directory_after.as_ref() != Some(&directory_before)
        || path_chain_contains_symlink(path)
    {
        snapshot.finding(
            FindingClass::Changed,
            role,
            relative,
            "directory_changed_during_inventory",
        );
    }
}

fn scan_root(
    root: &RootSpec,
    snapshot: &mut InventorySnapshot,
    limits: InventoryLimits,
    hook: &mut impl FnMut(InventoryEvent<'_>) -> bool,
) {
    match fs::symlink_metadata(&root.path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !root.required => {
            insert_entry(
                snapshot,
                limits,
                &root.role,
                ".",
                InventoryEntry {
                    kind: EntryKind::Absent,
                    bytes: 0,
                    sha256: None,
                },
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => snapshot.finding(
            FindingClass::Missing,
            &root.role,
            ".",
            "required_root_missing",
        ),
        Err(_) => snapshot.finding(FindingClass::Io, &root.role, ".", "root_metadata_failed"),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => snapshot
            .finding(
                FindingClass::Unsafe,
                &root.role,
                ".",
                "root_not_safe_directory",
            ),
        Ok(_) => scan_directory(
            &root.path,
            &root.role,
            ".",
            0,
            &root.exclusions,
            snapshot,
            limits,
            hook,
        ),
    }
}

fn canonical_uuid_suffix(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|uuid| uuid.to_string() == value)
}

fn discover_retired(
    base: &Path,
    kind: &str,
    snapshot: &mut InventorySnapshot,
) -> BTreeMap<String, PathBuf> {
    let mut discovered = BTreeMap::new();
    let Some(parent) = base.parent() else {
        snapshot.finding(
            FindingClass::Unsafe,
            "retired_namespace",
            kind,
            "base_has_no_parent",
        );
        return discovered;
    };
    let Some(name) = base.file_name().and_then(|name| name.to_str()) else {
        snapshot.finding(
            FindingClass::Unsafe,
            "retired_namespace",
            kind,
            "unsafe_base_name",
        );
        return discovered;
    };
    if path_chain_contains_symlink(parent) {
        snapshot.finding(
            FindingClass::Unsafe,
            "retired_namespace",
            kind,
            "parent_path_contains_symlink",
        );
        return discovered;
    }
    let parent_before = match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            match metadata_signature(&metadata) {
                Ok(signature) => signature,
                Err(reason) => {
                    snapshot.finding(FindingClass::Io, "retired_namespace", kind, reason);
                    return discovered;
                }
            }
        }
        Ok(_) => {
            snapshot.finding(
                FindingClass::Unsafe,
                "retired_namespace",
                kind,
                "parent_not_safe_directory",
            );
            return discovered;
        }
        Err(_) => {
            snapshot.finding(
                FindingClass::Io,
                "retired_namespace",
                kind,
                "parent_metadata_failed",
            );
            return discovered;
        }
    };
    let prefix = format!("{name}.retired-");
    let entries = match observed_directory(parent) {
        Ok(entries) => entries,
        Err(_) => {
            snapshot.finding(
                FindingClass::Io,
                "retired_namespace",
                kind,
                "parent_enumeration_failed",
            );
            return discovered;
        }
    };
    if entries.len() > MAX_TREE_ENTRIES {
        snapshot.finding(
            FindingClass::Limit,
            "retired_namespace",
            kind,
            "parent_entry_limit_exceeded",
        );
        return discovered;
    }
    for (entry_name, signature) in &entries {
        let Some(suffix) = entry_name.strip_prefix(&prefix) else {
            continue;
        };
        if !canonical_uuid_suffix(suffix) {
            snapshot.finding(
                FindingClass::Unexpected,
                "retired_namespace",
                kind,
                "noncanonical_retired_epoch_name",
            );
            continue;
        }
        let path = parent.join(entry_name);
        if path_chain_contains_symlink(parent)
            || fs::symlink_metadata(parent)
                .ok()
                .and_then(|metadata| metadata_signature(&metadata).ok())
                .as_ref()
                != Some(&parent_before)
            || fs::symlink_metadata(&path)
                .ok()
                .and_then(|metadata| metadata_signature(&metadata).ok())
                .as_ref()
                != Some(signature)
        {
            snapshot.finding(
                FindingClass::Changed,
                "retired_namespace",
                kind,
                "retired_entry_changed_during_discovery",
            );
            continue;
        }
        discovered.insert(suffix.to_owned(), path);
    }
    if path_chain_contains_symlink(parent)
        || fs::symlink_metadata(parent)
            .ok()
            .and_then(|metadata| metadata_signature(&metadata).ok())
            .as_ref()
            != Some(&parent_before)
        || observed_directory(parent).as_ref() != Ok(&entries)
    {
        snapshot.finding(
            FindingClass::Changed,
            "retired_namespace",
            kind,
            "retired_namespace_changed_during_discovery",
        );
    }
    discovered
}

fn build_roots(config: &PreflightConfig, snapshot: &mut InventorySnapshot) -> Vec<RootSpec> {
    let library_retired = discover_retired(&config.library_root, "library", snapshot);
    let sync_retired = discover_retired(&config.sync_root, "sync", snapshot);
    let suffixes: BTreeSet<_> = library_retired
        .keys()
        .chain(sync_retired.keys())
        .cloned()
        .collect();
    for suffix in &suffixes {
        if !library_retired.contains_key(suffix) || !sync_retired.contains_key(suffix) {
            snapshot.finding(
                FindingClass::Unexpected,
                "retired_namespace",
                suffix,
                "unpaired_retired_epoch",
            );
        }
    }
    let library_adopt = config.library_root.with_file_name(format!(
        "{}.adopt",
        config.library_root.file_name().unwrap().to_string_lossy()
    ));
    let sync_adopt = config.sync_root.with_file_name(format!(
        "{}.adopt",
        config.sync_root.file_name().unwrap().to_string_lossy()
    ));
    let sync_adopt_lock = config.sync_root.with_file_name(format!(
        "{}.adopt.lock",
        config.sync_root.file_name().unwrap().to_string_lossy()
    ));
    let mut state_exclusions = BTreeMap::from([
        (config.sync_root.clone(), "sync".to_owned()),
        (config.device_root.clone(), "device".to_owned()),
        (sync_adopt.clone(), "sync_adopt".to_owned()),
    ]);
    state_exclusions.insert(sync_adopt_lock, "excluded_lock".to_owned());
    state_exclusions.insert(
        config.state_root.join("replica.lock"),
        "excluded_lock".to_owned(),
    );
    for (suffix, path) in &sync_retired {
        state_exclusions.insert(path.clone(), format!("retired_sync:{suffix}"));
    }
    let mut roots = vec![
        RootSpec {
            role: "library".into(),
            path: config.library_root.clone(),
            required: true,
            exclusions: BTreeMap::from([(
                config.library_root.join(".scribe-sync/identity.lock"),
                "excluded_lock".to_owned(),
            )]),
        },
        RootSpec {
            role: "sync".into(),
            path: config.sync_root.clone(),
            required: false,
            exclusions: BTreeMap::from([
                (
                    config.sync_root.join("sync.lock"),
                    "excluded_lock".to_owned(),
                ),
                (
                    config.sync_root.join("replica.lock"),
                    "excluded_lock".to_owned(),
                ),
            ]),
        },
        RootSpec {
            role: "device".into(),
            path: config.device_root.clone(),
            required: false,
            exclusions: BTreeMap::new(),
        },
        RootSpec {
            role: "state".into(),
            path: config.state_root.clone(),
            required: true,
            exclusions: state_exclusions,
        },
        RootSpec {
            role: "activation_run".into(),
            path: config.app_root.join("run"),
            required: false,
            exclusions: BTreeMap::new(),
        },
        RootSpec {
            role: "library_adopt".into(),
            path: library_adopt.clone(),
            required: false,
            exclusions: BTreeMap::from([(
                library_adopt.join(".scribe-sync/identity.lock"),
                "excluded_lock".to_owned(),
            )]),
        },
        RootSpec {
            role: "sync_adopt".into(),
            path: sync_adopt.clone(),
            required: false,
            exclusions: BTreeMap::from([
                (
                    sync_adopt.join("sync/sync.lock"),
                    "excluded_lock".to_owned(),
                ),
                (
                    sync_adopt.join("sync/replica.lock"),
                    "excluded_lock".to_owned(),
                ),
            ]),
        },
    ];
    for suffix in suffixes {
        if let Some(path) = library_retired.get(&suffix) {
            roots.push(RootSpec {
                role: format!("retired_library:{suffix}"),
                path: path.clone(),
                required: true,
                exclusions: BTreeMap::from([(
                    path.join(".scribe-sync/identity.lock"),
                    "excluded_lock".to_owned(),
                )]),
            });
        }
        if let Some(path) = sync_retired.get(&suffix) {
            roots.push(RootSpec {
                role: format!("retired_sync:{suffix}"),
                path: path.clone(),
                required: true,
                exclusions: BTreeMap::from([
                    (path.join("sync.lock"), "excluded_lock".to_owned()),
                    (path.join("replica.lock"), "excluded_lock".to_owned()),
                ]),
            });
        }
    }
    roots.sort_by(|left, right| left.role.cmp(&right.role));
    roots
}

pub fn collect_inventory(config: &PreflightConfig) -> InventorySnapshot {
    collect_inventory_with(config, InventoryLimits::default(), &mut |_| true)
}

fn collect_inventory_with(
    config: &PreflightConfig,
    limits: InventoryLimits,
    hook: &mut impl FnMut(InventoryEvent<'_>) -> bool,
) -> InventorySnapshot {
    let mut snapshot = InventorySnapshot::new();
    if validate_config(config).is_err() {
        snapshot.finding(
            FindingClass::Unsafe,
            "configuration",
            ".",
            "invalid_fixed_root_configuration",
        );
        snapshot.finish();
        return snapshot;
    }
    let roots = build_roots(config, &mut snapshot);
    let mut seen = BTreeSet::new();
    for root in &roots {
        if !seen.insert(root.path.clone()) {
            snapshot.finding(
                FindingClass::Unexpected,
                &root.role,
                ".",
                "duplicate_root_definition",
            );
            continue;
        }
        scan_root(root, &mut snapshot, limits, hook);
    }
    snapshot.finish();
    snapshot
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NormalizedInspection {
    Passed,
    Issues,
    Incomplete,
}

impl NormalizedInspection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Issues => "issues",
            Self::Incomplete => "incomplete",
        }
    }
}

fn normalize_library(
    report: &crate::storage_inspect_library::LibraryInspectionReport,
) -> NormalizedInspection {
    match report.result_token() {
        "VALID" => NormalizedInspection::Passed,
        "ISSUES_DETECTED" => NormalizedInspection::Issues,
        _ => NormalizedInspection::Incomplete,
    }
}

fn normalize_external(
    report: &crate::storage_inspect_external::ExternalInspectionReport,
) -> NormalizedInspection {
    match report.result_token() {
        "VALID" | "VALID_PENDING" => NormalizedInspection::Passed,
        "ISSUES_DETECTED" => NormalizedInspection::Issues,
        _ => NormalizedInspection::Incomplete,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreflightReport {
    first: InventorySnapshot,
    second: InventorySnapshot,
    first_digest: Option<String>,
    second_digest: Option<String>,
    library_result: String,
    library_normalized: NormalizedInspection,
    external_result: String,
    external_normalized: NormalizedInspection,
}

impl PreflightReport {
    fn observational_stability(&self) -> &'static str {
        if self.first_digest.is_some()
            && self.second_digest.is_some()
            && self.first_digest != self.second_digest
        {
            "changed"
        } else if !self.first.complete() || !self.second.complete() {
            "unknown"
        } else {
            "equal"
        }
    }

    fn result_token(&self) -> &'static str {
        if self.observational_stability() == "changed"
            || self.first.has_class(FindingClass::Changed)
            || self.second.has_class(FindingClass::Changed)
        {
            "STATE_CHANGED"
        } else if self.first.has_class(FindingClass::Unsafe)
            || self.second.has_class(FindingClass::Unsafe)
        {
            "UNSAFE_INPUT"
        } else if self.first.has_class(FindingClass::Interrupted)
            || self.second.has_class(FindingClass::Interrupted)
        {
            "INTERRUPTED"
        } else if self.first.has_class(FindingClass::Limit)
            || self.second.has_class(FindingClass::Limit)
        {
            "LIMIT_EXCEEDED"
        } else if !self.first.complete()
            || !self.second.complete()
            || self.library_normalized == NormalizedInspection::Incomplete
            || self.external_normalized == NormalizedInspection::Incomplete
        {
            "INCOMPLETE"
        } else {
            "REVIEW_REQUIRED"
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self.result_token() {
            "STATE_CHANGED" => 4,
            "UNSAFE_INPUT" | "INTERRUPTED" | "LIMIT_EXCEEDED" => 3,
            _ => 2,
        }
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, String> {
        let mut output = String::new();
        output.push_str("IGNITE_COMPATIBILITY_PREFLIGHT_V1\n");
        output.push_str(&format!("schema={REPORT_SCHEMA}\n"));
        output.push_str(&format!("result={}\n", self.result_token()));
        output.push_str(&format!(
            "inventory_coverage={}\n",
            if self.first.complete() && self.second.complete() {
                "complete"
            } else {
                "incomplete"
            }
        ));
        output.push_str(&format!(
            "inventory_first_result={}\n",
            if self.first.complete() {
                "complete"
            } else {
                "incomplete"
            }
        ));
        output.push_str(&format!(
            "inventory_first_sha256={}\n",
            self.first_digest.as_deref().unwrap_or("unavailable")
        ));
        output.push_str(&format!(
            "inventory_second_result={}\n",
            if self.second.complete() {
                "complete"
            } else {
                "incomplete"
            }
        ));
        output.push_str(&format!(
            "inventory_second_sha256={}\n",
            self.second_digest.as_deref().unwrap_or("unavailable")
        ));
        output.push_str(&format!(
            "inventory_first_entries={}\n",
            self.first.entries.len()
        ));
        output.push_str(&format!(
            "inventory_second_entries={}\n",
            self.second.entries.len()
        ));
        output.push_str(&format!(
            "inventory_first_bytes={}\n",
            self.first.bytes_hashed
        ));
        output.push_str(&format!(
            "inventory_second_bytes={}\n",
            self.second.bytes_hashed
        ));
        output.push_str(&format!(
            "observational_stability={}\n",
            self.observational_stability()
        ));
        output.push_str(&format!("library_inspection={}\n", self.library_result));
        output.push_str(&format!(
            "library_inspection_normalized={}\n",
            self.library_normalized.as_str()
        ));
        output.push_str(&format!("external_inspection={}\n", self.external_result));
        output.push_str(&format!(
            "external_inspection_normalized={}\n",
            self.external_normalized.as_str()
        ));
        output.push_str("writer_exclusion=unverified\n");
        output.push_str("installation_binding=unverified\n");
        output.push_str("exact_a_probe=unavailable\n");
        output.push_str("exact_b_probe=unavailable\n");
        output.push_str("future_write_compatibility=unverified\n");
        output.push_str("bidirectional_compatibility=unavailable\n");
        output.push_str("atomic_snapshot=false\n");
        output.push_str("data_compatibility=unknown\n");
        output.push_str("promotion_authorized=false\n");
        output.push_str("inspection_action=report-only\n");
        for (pass, snapshot) in [("first", &self.first), ("second", &self.second)] {
            output.push_str(&format!(
                "inventory_{pass}_finding_count={}\n",
                snapshot.findings.len()
            ));
            for finding in &snapshot.findings {
                output.push_str(&format!(
                    "inventory_{pass}_finding={}|{}|{}|{}\n",
                    finding.class.as_str(),
                    finding.role,
                    finding.relative_path,
                    finding.reason
                ));
            }
        }
        if output.len() > REPORT_MAX_BYTES {
            return Err(format!("preflight report exceeds {REPORT_MAX_BYTES} bytes"));
        }
        Ok(output.into_bytes())
    }
}

pub fn inspect_preflight(config: &PreflightConfig) -> PreflightReport {
    inspect_preflight_with_hook(config, || {})
}

fn inspect_preflight_with_hook(
    config: &PreflightConfig,
    between_passes: impl FnOnce(),
) -> PreflightReport {
    let first = collect_inventory(config);
    let library = crate::storage_inspect_library::inspect_library(&config.library_root);
    let external = crate::storage_inspect_external::inspect_external(
        &crate::storage_inspect_external::ExternalInspectionConfig {
            library_root: config.library_root.clone(),
            sync_root: config.sync_root.clone(),
            device_root: config.device_root.clone(),
            state_root: config.state_root.clone(),
        },
    );
    between_passes();
    let second = collect_inventory(config);
    let first_digest = first.digest().ok();
    let second_digest = second.digest().ok();
    PreflightReport {
        first,
        second,
        first_digest,
        second_digest,
        library_result: library.result_token().to_owned(),
        library_normalized: normalize_library(&library),
        external_result: external.result_token().to_owned(),
        external_normalized: normalize_external(&external),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    const OWNER_FILE: &str = ".ignite-test-root-owner";
    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct OwnedTestRoot {
        path: PathBuf,
        token: String,
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
    }

    impl OwnedTestRoot {
        fn new(prefix: &str) -> Self {
            for _ in 0..128 {
                let path = std::env::temp_dir().join(format!(
                    "{prefix}-{}-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed),
                    uuid::Uuid::now_v7()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => {
                        let root_metadata = fs::symlink_metadata(&path).unwrap();
                        assert!(root_metadata.is_dir());
                        assert!(!root_metadata.file_type().is_symlink());
                        let token = uuid::Uuid::now_v7().to_string();
                        let owner = path.join(OWNER_FILE);
                        let mut file = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&owner)
                            .unwrap();
                        file.write_all(token.as_bytes()).unwrap();
                        return Self {
                            path,
                            token,
                            #[cfg(unix)]
                            device: {
                                use std::os::unix::fs::MetadataExt as _;
                                root_metadata.dev()
                            },
                            #[cfg(unix)]
                            inode: {
                                use std::os::unix::fs::MetadataExt as _;
                                root_metadata.ino()
                            },
                        };
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create {}: {error}", path.display()),
                }
            }
            panic!("no exclusive preflight test root was available");
        }
    }

    impl Drop for OwnedTestRoot {
        fn drop(&mut self) {
            let owner = self.path.join(OWNER_FILE);
            let root_owned = fs::symlink_metadata(&self.path)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
            #[cfg(unix)]
            let root_owned = root_owned
                && fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
                    use std::os::unix::fs::MetadataExt as _;
                    metadata.dev() == self.device && metadata.ino() == self.inode
                });
            let marker_owned = fs::symlink_metadata(&owner)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
            #[cfg(unix)]
            let marker_owned = marker_owned
                && fs::symlink_metadata(&owner).is_ok_and(|metadata| {
                    use std::os::unix::fs::MetadataExt as _;
                    metadata.nlink() == 1
                });
            let owned = root_owned
                && marker_owned
                && fs::read_to_string(&owner)
                    .is_ok_and(|contents| contents.as_str() == self.token.as_str());
            if owned {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }

    struct Fixture {
        _owner: OwnedTestRoot,
        config: PreflightConfig,
    }

    impl Fixture {
        fn new() -> Self {
            let owner = OwnedTestRoot::new("ignite-preflight");
            let app = owner.path.join("app");
            let state = app.join("state");
            let sync = state.join("sync");
            let device = state.join("device");
            let library = owner.path.join("library");
            for path in [&app, &state, &sync, &device, &library] {
                fs::create_dir(path).unwrap();
            }
            fs::write(library.join("library.meta"), b"library\n").unwrap();
            fs::write(sync.join("outbox.jsonl"), b"").unwrap();
            fs::write(device.join("preferences.json"), b"{}\n").unwrap();
            Self {
                config: PreflightConfig {
                    app_root: app,
                    library_root: library,
                    sync_root: sync,
                    device_root: device,
                    state_root: state,
                },
                _owner: owner,
            }
        }

        fn file(&self) -> PathBuf {
            self.config.library_root.join("library.meta")
        }
    }

    #[test]
    fn command_is_exact_and_rejects_relative_traversal_and_conflicting_roots() {
        let fixture = Fixture::new();
        let valid = vec![
            "--inspect-compatibility-preflight".into(),
            "--app-root".into(),
            fixture.config.app_root.to_string_lossy().into_owned(),
            "--data-dir".into(),
            fixture.config.library_root.to_string_lossy().into_owned(),
            "--sync-dir".into(),
            fixture.config.sync_root.to_string_lossy().into_owned(),
            "--device-dir".into(),
            fixture.config.device_root.to_string_lossy().into_owned(),
            "--state-dir".into(),
            fixture.config.state_root.to_string_lossy().into_owned(),
        ];
        assert!(matches!(
            parse_command(&valid).unwrap(),
            InspectionCommand::Inspect(_)
        ));
        for invalid in [
            valid[..10].to_vec(),
            [valid.clone(), valid.clone()].concat(),
            {
                let mut args = valid.clone();
                args[4] = "relative".into();
                args
            },
            {
                let mut args = valid.clone();
                args[4] = fixture
                    .config
                    .library_root
                    .join("..")
                    .join("other")
                    .to_string_lossy()
                    .into_owned();
                args
            },
            {
                let mut args = valid.clone();
                args[8] = args[6].clone();
                args
            },
        ] {
            assert!(parse_command(&invalid).is_err());
        }
    }

    #[test]
    fn identical_inventories_are_deterministic_and_preserve_bytes() {
        let fixture = Fixture::new();
        let before = fs::read(fixture.file()).unwrap();
        let first = collect_inventory(&fixture.config);
        let second = collect_inventory(&fixture.config);
        assert!(first.complete(), "{:?}", first.findings);
        assert_eq!(first, second);
        assert_eq!(first.digest().unwrap(), second.digest().unwrap());
        assert_eq!(
            first.canonical_bytes().unwrap(),
            second.canonical_bytes().unwrap()
        );
        assert_eq!(fs::read(fixture.file()).unwrap(), before);
    }

    #[test]
    fn additions_removals_renames_and_type_substitutions_change_the_second_root() {
        for mutation in ["add", "remove", "rename", "file_to_directory"] {
            let fixture = Fixture::new();
            let original = fixture.file();
            let report = inspect_preflight_with_hook(&fixture.config, || match mutation {
                "add" => fs::write(fixture.config.library_root.join("added"), b"new").unwrap(),
                "remove" => fs::remove_file(&original).unwrap(),
                "rename" => {
                    fs::rename(&original, fixture.config.library_root.join("renamed.meta")).unwrap()
                }
                "file_to_directory" => {
                    fs::remove_file(&original).unwrap();
                    fs::create_dir(&original).unwrap();
                }
                _ => unreachable!(),
            });
            assert_eq!(report.observational_stability(), "changed", "{mutation}");
            assert_eq!(report.result_token(), "STATE_CHANGED", "{mutation}");
            assert_eq!(report.exit_code(), 4);
        }
    }

    #[test]
    fn same_size_replacement_is_detected_between_passes() {
        let fixture = Fixture::new();
        let original = fixture.file();
        let report = inspect_preflight_with_hook(&fixture.config, || {
            fs::write(&original, b"LIBRARY\n").unwrap();
        });
        assert_eq!(report.observational_stability(), "changed");
        assert_eq!(report.result_token(), "STATE_CHANGED");
    }

    #[test]
    fn concurrent_path_replacement_after_open_invalidates_inventory() {
        let fixture = Fixture::new();
        let target = fixture.file();
        let displaced = fixture.config.library_root.join("displaced.meta");
        let mut replaced = false;
        let snapshot =
            collect_inventory_with(&fixture.config, InventoryLimits::default(), &mut |event| {
                if !replaced && event == InventoryEvent::FileOpened(&target) {
                    fs::rename(&target, &displaced).unwrap();
                    fs::write(&target, b"replacement\n").unwrap();
                    replaced = true;
                }
                true
            });
        assert!(!snapshot.complete());
        assert!(snapshot.has_class(FindingClass::Changed));
    }

    #[test]
    fn missing_required_and_absent_optional_roots_are_distinguished() {
        let optional = Fixture::new();
        fs::remove_file(optional.config.sync_root.join("outbox.jsonl")).unwrap();
        fs::remove_dir(&optional.config.sync_root).unwrap();
        fs::remove_file(optional.config.device_root.join("preferences.json")).unwrap();
        fs::remove_dir(&optional.config.device_root).unwrap();
        let snapshot = collect_inventory(&optional.config);
        assert!(snapshot.complete(), "{:?}", snapshot.findings);
        assert_eq!(
            snapshot
                .entries
                .get(&("sync".into(), ".".into()))
                .unwrap()
                .kind,
            EntryKind::Absent
        );

        let required = Fixture::new();
        fs::remove_file(required.file()).unwrap();
        fs::remove_dir(&required.config.library_root).unwrap();
        let snapshot = collect_inventory(&required.config);
        assert!(!snapshot.complete());
        assert!(snapshot.has_class(FindingClass::Missing));
    }

    #[test]
    fn malformed_reserved_namespace_and_conflicting_api_config_fail_closed() {
        let fixture = Fixture::new();
        fs::write(
            fixture
                .config
                .library_root
                .join("unknown-persistent-object"),
            b"preserve",
        )
        .unwrap();
        fs::create_dir(
            fixture
                .config
                .library_root
                .with_file_name("library.retired-not-a-uuid"),
        )
        .unwrap();
        let unexpected = collect_inventory(&fixture.config);
        assert!(unexpected.has_class(FindingClass::Unexpected));

        let mut conflicting = fixture.config.clone();
        conflicting.device_root = conflicting.sync_root.clone();
        let invalid = collect_inventory(&conflicting);
        assert!(invalid.has_class(FindingClass::Unsafe));
        assert!(invalid.entries.is_empty());
    }

    #[test]
    fn entry_depth_and_byte_limits_are_explicit() {
        let entries = Fixture::new();
        fs::write(entries.config.library_root.join("extra"), b"x").unwrap();
        let limited = collect_inventory_with(
            &entries.config,
            InventoryLimits {
                entries: 2,
                ..InventoryLimits::default()
            },
            &mut |_| true,
        );
        assert!(limited.has_class(FindingClass::Limit));

        let depth = Fixture::new();
        fs::create_dir_all(depth.config.library_root.join("a/b/c")).unwrap();
        let limited = collect_inventory_with(
            &depth.config,
            InventoryLimits {
                depth: 1,
                ..InventoryLimits::default()
            },
            &mut |_| true,
        );
        assert!(limited.has_class(FindingClass::Limit));

        let bytes = Fixture::new();
        let limited = collect_inventory_with(
            &bytes.config,
            InventoryLimits {
                file_bytes: 2,
                total_bytes: 4,
                ..InventoryLimits::default()
            },
            &mut |_| true,
        );
        assert!(limited.has_class(FindingClass::Limit));
    }

    #[test]
    fn interrupted_collection_is_incomplete_without_retry() {
        let fixture = Fixture::new();
        let mut calls = 0;
        let snapshot =
            collect_inventory_with(&fixture.config, InventoryLimits::default(), &mut |_| {
                calls += 1;
                calls < 2
            });
        assert!(snapshot.has_class(FindingClass::Interrupted));
        assert!(!snapshot.complete());
    }

    #[test]
    fn enumeration_order_does_not_change_canonical_inventory() {
        let first = Fixture::new();
        let second = Fixture::new();
        for name in ["a", "b", "c"] {
            fs::write(first.config.library_root.join(name), name.as_bytes()).unwrap();
        }
        for name in ["c", "b", "a"] {
            fs::write(second.config.library_root.join(name), name.as_bytes()).unwrap();
        }
        assert_eq!(
            collect_inventory(&first.config).canonical_bytes().unwrap(),
            collect_inventory(&second.config).canonical_bytes().unwrap()
        );
    }

    #[test]
    fn preflight_never_reports_compatibility_or_authorization() {
        let fixture = Fixture::new();
        let report = inspect_preflight(&fixture.config);
        assert_ne!(report.exit_code(), 0);
        let bytes = String::from_utf8(report.canonical_bytes().unwrap()).unwrap();
        assert!(bytes.contains("writer_exclusion=unverified\n"));
        assert!(bytes.contains("installation_binding=unverified\n"));
        assert!(bytes.contains("bidirectional_compatibility=unavailable\n"));
        assert!(bytes.contains("atomic_snapshot=false\n"));
        assert!(bytes.contains("data_compatibility=unknown\n"));
        assert!(bytes.contains("promotion_authorized=false\n"));
        assert!(bytes.contains("inspection_action=report-only\n"));
        assert!(!bytes.contains("BIDIRECTIONAL_COMPATIBLE"));
    }

    #[test]
    fn incomplete_underlying_inspector_remains_incomplete() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.file()).unwrap();
        fs::remove_dir(&fixture.config.library_root).unwrap();
        let report = inspect_preflight(&fixture.config);
        assert_eq!(report.library_normalized, NormalizedInspection::Incomplete);
        assert_ne!(report.exit_code(), 0);
        assert!(String::from_utf8(report.canonical_bytes().unwrap())
            .unwrap()
            .contains("library_inspection_normalized=incomplete\n"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hard_link_and_special_file_inputs_are_refused() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let symlinked = Fixture::new();
        let target = symlinked.config.library_root.join("target");
        fs::write(&target, b"target").unwrap();
        symlink(&target, symlinked.config.library_root.join("alias")).unwrap();
        assert!(collect_inventory(&symlinked.config).has_class(FindingClass::Unsafe));

        let linked = Fixture::new();
        let target = linked.config.library_root.join("target");
        fs::write(&target, b"target").unwrap();
        fs::hard_link(&target, linked.config.library_root.join("alias")).unwrap();
        assert!(collect_inventory(&linked.config).has_class(FindingClass::Unsafe));

        let special = Fixture::new();
        let _listener = UnixListener::bind(special.config.library_root.join("socket")).unwrap();
        assert!(collect_inventory(&special.config).has_class(FindingClass::Unsafe));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_parent_is_refused_before_retired_discovery_or_file_open() {
        use std::os::unix::fs::symlink;

        let owner = OwnedTestRoot::new("ignite-preflight-parent-symlink");
        let real = owner.path.join("real");
        let app = real.join("app");
        let state = app.join("state");
        let sync = state.join("sync");
        let device = state.join("device");
        let library = real.join("library");
        for path in [&app, &state, &sync, &device, &library] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(library.join("library.meta"), b"must-not-be-opened").unwrap();
        let alias = owner.path.join("alias");
        symlink(&real, &alias).unwrap();
        let config = PreflightConfig {
            app_root: alias.join("app"),
            state_root: alias.join("app/state"),
            sync_root: alias.join("app/state/sync"),
            device_root: alias.join("app/state/device"),
            library_root: alias.join("library"),
        };
        let mut opened = false;
        let snapshot = collect_inventory_with(&config, InventoryLimits::default(), &mut |event| {
            opened |= matches!(event, InventoryEvent::FileOpened(_));
            true
        });

        assert!(snapshot.has_class(FindingClass::Unsafe));
        assert!(!opened);
        assert_eq!(
            fs::read(library.join("library.meta")).unwrap(),
            b"must-not-be-opened"
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_substitution_during_scan_is_never_traversed() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let nested = fixture.config.library_root.join("nested");
        let displaced = fixture.config.library_root.join("nested.displaced");
        let outside = fixture._owner.path.join("outside");
        fs::create_dir(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(nested.join("inside"), b"original").unwrap();
        fs::write(outside.join("inside"), b"must-not-be-read").unwrap();

        let mut substituted = false;
        let snapshot =
            collect_inventory_with(&fixture.config, InventoryLimits::default(), &mut |event| {
                if !substituted && event == InventoryEvent::DirectoryEnumerated(&nested) {
                    fs::rename(&nested, &displaced).unwrap();
                    symlink(&outside, &nested).unwrap();
                    substituted = true;
                }
                true
            });

        assert!(substituted);
        assert!(!snapshot.complete());
        assert!(snapshot.has_class(FindingClass::Changed));
        assert!(!snapshot
            .entries
            .contains_key(&("library".into(), "nested/inside".into())));
        assert_eq!(
            fs::read(outside.join("inside")).unwrap(),
            b"must-not-be-read"
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_to_symlink_substitution_is_never_traversed() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = fixture._owner.path.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"preserve").unwrap();
        let nested = fixture.config.library_root.join("nested");
        fs::create_dir(&nested).unwrap();
        let report = inspect_preflight_with_hook(&fixture.config, || {
            fs::remove_dir(&nested).unwrap();
            symlink(&outside, &nested).unwrap();
        });
        assert_eq!(report.result_token(), "STATE_CHANGED");
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"preserve");
    }
}
