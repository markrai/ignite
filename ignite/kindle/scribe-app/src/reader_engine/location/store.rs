use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use super::{DocumentIdentity, PositionCodecError, SavedPosition, MAX_SAVED_POSITION_BYTES};

const SLOT_FORMAT: u16 = 1;
const MAX_SLOT_BYTES: usize = MAX_SAVED_POSITION_BYTES + 512;
const CHECKSUM_DOMAIN: &[u8] = b"ignite-position-slot-v1\0";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct PositionStore {
    root: PathBuf,
}

#[derive(Debug)]
pub enum PositionStoreError {
    Io(io::Error),
    Codec(PositionCodecError),
    UnsafePath,
    Corrupt,
    ConflictingGenerations,
    UnsupportedStoreFormat(u16),
    UnsupportedPositionFormat(u16),
    GenerationExhausted,
    UnsupportedPlatform,
}

impl std::fmt::Display for PositionStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "reading-position I/O: {error}"),
            Self::Codec(error) => write!(f, "reading-position encoding: {error}"),
            Self::UnsafePath => write!(f, "unsafe reading-position store path"),
            Self::Corrupt => write!(f, "no valid reading-position record remains"),
            Self::ConflictingGenerations => write!(f, "conflicting reading-position generations"),
            Self::UnsupportedStoreFormat(version) => {
                write!(f, "unsupported reading-position store format {version}")
            }
            Self::UnsupportedPositionFormat(version) => {
                write!(f, "unsupported saved-position format {version}")
            }
            Self::GenerationExhausted => write!(f, "reading-position generation exhausted"),
            Self::UnsupportedPlatform => write!(f, "durable position writes require Unix"),
        }
    }
}

impl std::error::Error for PositionStoreError {}

impl From<io::Error> for PositionStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PositionCodecError> for PositionStoreError {
    fn from(error: PositionCodecError) -> Self {
        Self::Codec(error)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlotWire {
    slot_format: u16,
    generation: u64,
    checksum: String,
    position: Box<RawValue>,
}

#[derive(Deserialize)]
struct SlotHeader {
    slot_format: u16,
}

struct ValidSlot {
    generation: u64,
    position: SavedPosition,
}

enum SlotStatus {
    Missing,
    Invalid,
    Unsupported(u16),
    UnsupportedPosition(u16),
    Valid(ValidSlot),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FailurePoint {
    None,
    AfterWrite,
    AfterFileSync,
    AfterRename,
}

struct TempGuard(PathBuf);

impl Drop for TempGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
struct WriterLock {
    _file: File,
}

#[cfg(unix)]
impl WriterLock {
    fn acquire(root: &Path) -> Result<Self, PositionStoreError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let file = options.open(root.join(".writer-lock"))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self { _file: file })
    }
}

impl PositionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn load(
        &self,
        identity: &DocumentIdentity,
    ) -> Result<Option<SavedPosition>, PositionStoreError> {
        if !self.directory_exists(false)? {
            return Ok(None);
        }
        let slots = self.read_slots(identity)?;
        Ok(Self::newest(&slots)?.map(|(_, slot)| slot.position.clone()))
    }

    pub fn save(&self, position: &SavedPosition) -> Result<u64, PositionStoreError> {
        self.save_with_failure(position, FailurePoint::None)
    }

    fn save_with_failure(
        &self,
        position: &SavedPosition,
        failure: FailurePoint,
    ) -> Result<u64, PositionStoreError> {
        let payload = position.to_json_bytes()?;
        if !self.directory_exists(true)? {
            return Err(PositionStoreError::UnsafePath);
        }
        #[cfg(unix)]
        let _lock = WriterLock::acquire(&self.root)?;
        #[cfg(not(unix))]
        return Err(PositionStoreError::UnsupportedPlatform);

        let identity = &position.document;
        let slots = self.read_slots(identity)?;
        let (target, generation) = match Self::newest(&slots)? {
            Some((latest, slot)) => (
                1 - latest,
                slot.generation
                    .checked_add(1)
                    .ok_or(PositionStoreError::GenerationExhausted)?,
            ),
            None => (0, 1),
        };
        let record = encode_slot(generation, &payload)?;
        let prefix = key(identity);
        let target_path = self.root.join(format!("{prefix}.slot{target}"));
        let (temporary, mut file) = self.create_temporary(&prefix, target)?;
        let guard = TempGuard(temporary.clone());

        file.write_all(&record)?;
        if failure == FailurePoint::AfterWrite {
            return Err(injected_error());
        }
        file.sync_all()?;
        if failure == FailurePoint::AfterFileSync {
            return Err(injected_error());
        }
        drop(file);
        fs::rename(&temporary, &target_path)?;
        if failure == FailurePoint::AfterRename {
            return Err(injected_error());
        }
        File::open(&self.root)?.sync_all()?;
        drop(guard);
        Ok(generation)
    }

    fn directory_exists(&self, create: bool) -> Result<bool, PositionStoreError> {
        match fs::symlink_metadata(&self.root) {
            Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
            Ok(_) => Err(PositionStoreError::UnsafePath),
            Err(error) if error.kind() == io::ErrorKind::NotFound && !create => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    let parent = self.root.parent().ok_or(PositionStoreError::UnsafePath)?;
                    let mut builder = fs::DirBuilder::new();
                    builder.mode(0o700);
                    match builder.create(&self.root) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            if !fs::symlink_metadata(&self.root)?.file_type().is_dir() {
                                return Err(PositionStoreError::UnsafePath);
                            }
                        }
                        Err(error) => return Err(error.into()),
                    }
                    File::open(parent)?.sync_all()?;
                    Ok(true)
                }
                #[cfg(not(unix))]
                {
                    Err(PositionStoreError::UnsupportedPlatform)
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn read_slots(
        &self,
        identity: &DocumentIdentity,
    ) -> Result<[SlotStatus; 2], PositionStoreError> {
        let prefix = key(identity);
        Ok([
            self.read_slot(&self.root.join(format!("{prefix}.slot0")), identity)?,
            self.read_slot(&self.root.join(format!("{prefix}.slot1")), identity)?,
        ])
    }

    fn read_slot(
        &self,
        path: &Path,
        identity: &DocumentIdentity,
    ) -> Result<SlotStatus, PositionStoreError> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SlotStatus::Missing);
            }
            Err(error) => return Err(error.into()),
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(PositionStoreError::UnsafePath);
            }
            Ok(_) => {}
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let file = options.open(path)?;
        let mut bytes = Vec::new();
        file.take((MAX_SLOT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_SLOT_BYTES {
            return Ok(SlotStatus::Invalid);
        }
        Ok(match decode_slot(&bytes, identity) {
            Ok(valid) => SlotStatus::Valid(valid),
            Err(SlotIssue::Invalid) => SlotStatus::Invalid,
            Err(SlotIssue::Unsupported(version)) => SlotStatus::Unsupported(version),
            Err(SlotIssue::UnsupportedPosition(version)) => {
                SlotStatus::UnsupportedPosition(version)
            },
        })
    }

    fn newest(slots: &[SlotStatus; 2]) -> Result<Option<(usize, &ValidSlot)>, PositionStoreError> {
        let mut latest: Option<(usize, &ValidSlot)> = None;
        let mut corrupt = false;
        for (index, status) in slots.iter().enumerate() {
            match status {
                SlotStatus::Missing => {}
                SlotStatus::Invalid => corrupt = true,
                SlotStatus::Unsupported(version) => {
                    return Err(PositionStoreError::UnsupportedStoreFormat(*version));
                }
                SlotStatus::UnsupportedPosition(version) => {
                    return Err(PositionStoreError::UnsupportedPositionFormat(*version));
                }
                SlotStatus::Valid(slot) => {
                    if let Some((_, previous)) = latest {
                        if previous.generation == slot.generation {
                            return Err(PositionStoreError::ConflictingGenerations);
                        }
                        if previous.generation > slot.generation {
                            continue;
                        }
                    }
                    latest = Some((index, slot));
                }
            }
        }
        if latest.is_none() && corrupt {
            return Err(PositionStoreError::Corrupt);
        }
        Ok(latest)
    }

    fn create_temporary(
        &self,
        prefix: &str,
        target: usize,
    ) -> Result<(PathBuf, File), PositionStoreError> {
        for _ in 0..32 {
            let number = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = self.root.join(format!(
                ".{prefix}.slot{target}.tmp-{}-{number}",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            match options.open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "temporary names exhausted").into())
    }
}

enum SlotIssue {
    Invalid,
    Unsupported(u16),
    UnsupportedPosition(u16),
}

fn decode_slot(bytes: &[u8], identity: &DocumentIdentity) -> Result<ValidSlot, SlotIssue> {
    let header: SlotHeader = serde_json::from_slice(bytes).map_err(|_| SlotIssue::Invalid)?;
    if header.slot_format != SLOT_FORMAT {
        return Err(SlotIssue::Unsupported(header.slot_format));
    }
    let wire: SlotWire = serde_json::from_slice(bytes).map_err(|_| SlotIssue::Invalid)?;
    if wire.generation == 0 {
        return Err(SlotIssue::Invalid);
    }
    let payload = wire.position.get().as_bytes();
    if payload.len() > MAX_SAVED_POSITION_BYTES
        || wire.checksum != checksum(wire.generation, payload)
    {
        return Err(SlotIssue::Invalid);
    }
    let position = match SavedPosition::from_json_bytes(payload) {
        Ok(position) => position,
        Err(PositionCodecError::UnsupportedFormat(version)) => {
            return Err(SlotIssue::UnsupportedPosition(version));
        }
        Err(_) => return Err(SlotIssue::Invalid),
    };
    if &position.document != identity {
        return Err(SlotIssue::Invalid);
    }
    Ok(ValidSlot {
        generation: wire.generation,
        position,
    })
}

fn encode_slot(generation: u64, payload: &[u8]) -> Result<Vec<u8>, PositionStoreError> {
    let raw = RawValue::from_string(
        String::from_utf8(payload.to_vec()).map_err(|_| PositionStoreError::Corrupt)?,
    )
    .map_err(|_| PositionStoreError::Corrupt)?;
    let wire = SlotWire {
        slot_format: SLOT_FORMAT,
        generation,
        checksum: checksum(generation, payload),
        position: raw,
    };
    let result = serde_json::to_vec(&wire).map_err(|_| PositionStoreError::Corrupt)?;
    if result.len() > MAX_SLOT_BYTES {
        return Err(PositionStoreError::Corrupt);
    }
    Ok(result)
}

fn checksum(generation: u64, payload: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CHECKSUM_DOMAIN);
    hasher.update(generation.to_be_bytes());
    hasher.update(payload);
    hex(&hasher.finalize())
}

fn key(identity: &DocumentIdentity) -> String {
    format!("{}-{}", hex(&identity.sha256), identity.len)
}

fn hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut result, "{byte:02x}").expect("write to String");
    }
    result
}

fn injected_error() -> PositionStoreError {
    io::Error::other("injected interruption").into()
}

#[cfg(test)]
mod tests;
