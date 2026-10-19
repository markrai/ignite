use std::fmt;

use portable_backup::{BackupError, BackupErrorCode};

/// ZIP container refusals (structure, features, resources, data integrity).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZipErrorCode {
    /// Too short, no end record at the very end, trailing data.
    NotZip,
    /// A record runs past the end of the file.
    Truncated,
    /// Inconsistent or invalid records.
    Malformed,
    /// A valid ZIP feature outside the accepted profile (method, flags, extra fields, …).
    Unsupported,
    Encrypted,
    /// Two entries with the same name.
    DuplicateEntry,
    /// Not a canonical logical backup path.
    InvalidEntryName,
    /// Symlink, directory or special file.
    NotRegularFile,
    /// Local header differs from the central directory.
    HeaderMismatch,
    /// Data before, between or after entries, or overlapping entries.
    Layout,
    MissingManifest,
    /// Declared counts or sizes beyond `Limits`.
    LimitExceeded,
    /// Inflated length differs from the declared size.
    SizeMismatch,
    CrcMismatch,
    /// Invalid deflate stream or trailing compressed bytes.
    CorruptData,
}

impl ZipErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotZip => "not_zip",
            Self::Truncated => "truncated",
            Self::Malformed => "malformed",
            Self::Unsupported => "unsupported",
            Self::Encrypted => "encrypted",
            Self::DuplicateEntry => "duplicate_entry",
            Self::InvalidEntryName => "invalid_entry_name",
            Self::NotRegularFile => "not_regular_file",
            Self::HeaderMismatch => "header_mismatch",
            Self::Layout => "layout",
            Self::MissingManifest => "missing_manifest",
            Self::LimitExceeded => "limit_exceeded",
            Self::SizeMismatch => "size_mismatch",
            Self::CrcMismatch => "crc_mismatch",
            Self::CorruptData => "corrupt_data",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreErrorKind {
    /// The archive file could not be opened or read.
    Io,
    /// The ZIP container was refused.
    Zip(ZipErrorCode),
    /// The logical backup was refused by `portable_backup::BackupVerifier`.
    Backup(BackupErrorCode),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreError {
    pub kind: RestoreErrorKind,
    pub message: String,
}

impl RestoreError {
    pub(crate) fn zip(code: ZipErrorCode, message: impl fmt::Display) -> Self {
        Self {
            kind: RestoreErrorKind::Zip(code),
            message: message.to_string(),
        }
    }

    pub(crate) fn io(message: impl fmt::Display) -> Self {
        Self {
            kind: RestoreErrorKind::Io,
            message: message.to_string(),
        }
    }
}

impl From<BackupError> for RestoreError {
    fn from(e: BackupError) -> Self {
        Self {
            kind: RestoreErrorKind::Backup(e.code),
            message: e.message,
        }
    }
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            RestoreErrorKind::Io => write!(f, "io: {}", self.message),
            RestoreErrorKind::Zip(c) => write!(f, "zip {}: {}", c.as_str(), self.message),
            RestoreErrorKind::Backup(c) => write!(f, "backup {c}: {}", self.message),
        }
    }
}

impl std::error::Error for RestoreError {}
