use std::fmt;

/// Deterministic refusal category. Messages add detail; callers branch on the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupErrorCode {
    /// An entry name is not one of the logical backup paths.
    InvalidPath,
    /// The manifest is malformed or structurally inconsistent.
    InvalidManifest,
    /// Unsupported backup format or snapshot version.
    UnsupportedVersion,
    /// Valid JSON that is not the exact canonical serialization.
    NonCanonical,
    /// A logical count or size bound was exceeded.
    LimitExceeded,
    /// An entry the manifest does not list.
    UnexpectedEntry,
    /// The same library, blob or entry appears twice.
    DuplicateEntry,
    /// A library listed in the manifest was never supplied.
    MissingEntry,
    /// Entry content length differs from the manifest declaration.
    SizeMismatch,
    /// Entry content hash differs from the manifest declaration.
    HashMismatch,
    /// The snapshot fails `sync-protocol` parsing or validation.
    InvalidSnapshot,
    /// `LibraryState::genesis` refused the snapshot.
    InvalidState,
    /// The snapshot's `library_id` differs from its path / manifest entry.
    LibraryMismatch,
    /// Notebook or page counts differ from the manifest.
    CountMismatch,
    /// A referenced blob is not listed, or its bytes were never supplied.
    MissingBlob,
    /// A listed blob no snapshot references.
    UnreferencedBlob,
    /// A blob's byte length disagrees between manifest and snapshot reference.
    BlobLengthMismatch,
    /// Re-keying was asked to keep the source library identity.
    SameLibrary,
    /// The entry reader failed.
    Io,
    /// The verifier already refused an earlier entry.
    Aborted,
}

impl BackupErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid_path",
            Self::InvalidManifest => "invalid_manifest",
            Self::UnsupportedVersion => "unsupported_version",
            Self::NonCanonical => "non_canonical",
            Self::LimitExceeded => "limit_exceeded",
            Self::UnexpectedEntry => "unexpected_entry",
            Self::DuplicateEntry => "duplicate_entry",
            Self::MissingEntry => "missing_entry",
            Self::SizeMismatch => "size_mismatch",
            Self::HashMismatch => "hash_mismatch",
            Self::InvalidSnapshot => "invalid_snapshot",
            Self::InvalidState => "invalid_state",
            Self::LibraryMismatch => "library_mismatch",
            Self::CountMismatch => "count_mismatch",
            Self::MissingBlob => "missing_blob",
            Self::UnreferencedBlob => "unreferenced_blob",
            Self::BlobLengthMismatch => "blob_length_mismatch",
            Self::SameLibrary => "same_library",
            Self::Io => "io",
            Self::Aborted => "aborted",
        }
    }
}

impl fmt::Display for BackupErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupError {
    pub code: BackupErrorCode,
    pub message: String,
}

impl BackupError {
    pub fn new(code: BackupErrorCode, message: impl fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BackupError {}

pub type Result<T> = std::result::Result<T, BackupError>;

pub(crate) fn fail<T>(code: BackupErrorCode, message: impl fmt::Display) -> Result<T> {
    Err(BackupError::new(code, message))
}
