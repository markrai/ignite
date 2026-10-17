//! Deterministic serialization and hashing.
//!
//! Canonical bytes = compact `serde_json` output of the typed DTO:
//! * struct fields in declaration order (fixed by the type, not by input order);
//! * every map is a `BTreeMap` (sorted keys);
//! * floats are f32 written as their shortest round-tripping decimal;
//! * optional fields are omitted when absent (never `null`);
//! * no whitespace, no trailing newline.
//!
//! Parsing is strict (`deny_unknown_fields`, typed IDs, explicit `v`), then validated.

use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::content::Sha256Hex;
use crate::error::{ProtocolError, Result};

/// Canonical compact JSON bytes of any sync DTO.
pub fn to_canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| ProtocolError::new(format!("serialize: {e}")))
}

/// Strict typed parse (no validation beyond the type; callers validate).
pub fn from_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| ProtocolError::new(format!("parse: {e}")))
}

pub fn sha256_hex(bytes: &[u8]) -> Sha256Hex {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    Sha256Hex::from_digest(&digest)
}
