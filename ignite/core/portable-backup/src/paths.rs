//! Logical entry paths. Only three shapes exist:
//!
//! ```text
//! manifest.json
//! libraries/<canonical LibraryId>/snapshot.json
//! blobs/<64 lowercase hex sha256>
//! ```
//!
//! This validates names only. It says nothing about ZIP metadata (symlinks, compression,
//! encryption, duplicate central-directory entries); the archive reader owns those checks.

use std::fmt;

use sync_protocol::{LibraryId, Sha256Hex};

use crate::error::{BackupError, BackupErrorCode, Result};

pub const MANIFEST_PATH: &str = "manifest.json";
/// Longer than every valid path (the longest, a snapshot path, is 60 bytes).
pub const MAX_ENTRY_PATH_LEN: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryPath {
    Manifest,
    Snapshot(LibraryId),
    Blob(Sha256Hex),
}

impl EntryPath {
    /// Strict parse. Identifiers must already be canonical (`LibraryId::parse`,
    /// `Sha256Hex::parse`): no uppercase, braces or alternative spellings.
    pub fn parse(path: &str) -> Result<Self> {
        let bad = |why: &str| {
            BackupError::new(
                BackupErrorCode::InvalidPath,
                format!("entry path {path:?}: {why}"),
            )
        };
        if path.is_empty() {
            return Err(bad("empty"));
        }
        if path.len() > MAX_ENTRY_PATH_LEN {
            return Err(bad("too long"));
        }
        if path.starts_with('/') {
            return Err(bad("absolute path"));
        }
        if path.contains('\\') {
            return Err(bad("backslash separator"));
        }
        if path.chars().any(char::is_control) {
            return Err(bad("control character"));
        }
        if path.contains(':') {
            return Err(bad("drive or scheme prefix"));
        }
        let parts: Vec<&str> = path.split('/').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(bad("empty path component"));
        }
        if parts.iter().any(|p| *p == "." || *p == "..") {
            return Err(bad("traversal component"));
        }
        match parts.as_slice() {
            [MANIFEST_PATH] => Ok(Self::Manifest),
            ["libraries", id, "snapshot.json"] => LibraryId::parse(id)
                .map(Self::Snapshot)
                .map_err(|e| bad(&format!("library id: {e}"))),
            ["blobs", sha] => Sha256Hex::parse(sha)
                .map(Self::Blob)
                .map_err(|e| bad(&format!("blob name: {e}"))),
            _ => Err(bad("not a recognized backup entry")),
        }
    }

    /// The canonical path text (round-trips through [`Self::parse`]).
    pub fn to_path(&self) -> String {
        match self {
            Self::Manifest => MANIFEST_PATH.to_string(),
            Self::Snapshot(id) => format!("libraries/{id}/snapshot.json"),
            Self::Blob(sha) => format!("blobs/{sha}"),
        }
    }
}

impl fmt::Display for EntryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIB: &str = "019a8ef2-3c4d-7abc-8def-0123456789ab";
    const SHA: &str = "4c2fd7a0b1e3c5d7f9a1b3c5d7e9f1a3b5c7d9e1f3a5b7c9d1e3f5a7b9c1d3e5";

    #[test]
    fn accepts_the_three_shapes_and_round_trips() {
        for p in [
            "manifest.json".to_string(),
            format!("libraries/{LIB}/snapshot.json"),
            format!("blobs/{SHA}"),
        ] {
            let e = EntryPath::parse(&p).unwrap();
            assert_eq!(e.to_path(), p);
            assert_eq!(EntryPath::parse(&e.to_string()).unwrap(), e);
        }
        assert_eq!(
            EntryPath::parse(&format!("libraries/{LIB}/snapshot.json")).unwrap(),
            EntryPath::Snapshot(LibraryId::parse(LIB).unwrap())
        );
    }

    #[test]
    fn refuses_everything_else() {
        let upper = LIB.to_uppercase();
        let sha_upper = SHA.to_uppercase();
        for p in [
            String::new(),
            "/manifest.json".into(),
            "./manifest.json".into(),
            "manifest.json/".into(),
            "Manifest.json".into(),
            "manifest.json\0".into(),
            "C:/manifest.json".into(),
            "c:manifest.json".into(),
            format!("libraries\\{LIB}\\snapshot.json"),
            format!("/libraries/{LIB}/snapshot.json"),
            format!("libraries/{LIB}/../{LIB}/snapshot.json"),
            format!("libraries/./{LIB}/snapshot.json"),
            format!("libraries//{LIB}/snapshot.json"),
            format!("libraries/{LIB}/snapshot.json/"),
            format!("libraries/{LIB}/history.json"),
            format!("libraries/{LIB}"),
            format!("libraries/{upper}/snapshot.json"),
            format!("libraries/{{{LIB}}}/snapshot.json"),
            "libraries/not-a-uuid/snapshot.json".into(),
            "libraries/00000000-0000-0000-0000-000000000000/snapshot.json".into(),
            format!("blobs/{sha_upper}"),
            format!("blobs/{}", &SHA[..63]),
            format!("blobs/{SHA}0"),
            format!("blobs/{SHA}/x"),
            format!("blobs/{SHA}.raw"),
            "blobs/..".into(),
            "blobs".into(),
            format!("extra/blobs/{SHA}"),
            "x".repeat(MAX_ENTRY_PATH_LEN + 1),
            format!("libraries/{LIB}/snapshot.json\n"),
        ] {
            let e = EntryPath::parse(&p).unwrap_err();
            assert_eq!(e.code, BackupErrorCode::InvalidPath, "{p:?}");
        }
    }
}
