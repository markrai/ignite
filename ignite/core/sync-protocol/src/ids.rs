//! Strongly typed global identities.
//!
//! Every sync identity is a UUID in canonical lowercase hyphenated form
//! (`xxxxxxxx-xxxx-Vxxx-Nxxx-xxxxxxxxxxxx`). Parsing is strict: braced, URN, simple (no
//! hyphens), uppercase, nil, max, non-RFC variants and versions other than 4, 5 or 7 are
//! refused. Newly minted IDs are UUIDv7 (feature `mint`); deterministic derivations are UUIDv5.
//!
//! Global IDs are independent of local storage names (`notebook-3`, `page-7`), display order,
//! titles, and the replica's active notebook/page.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::{Uuid, Variant};

use crate::error::{refuse, ProtocolError, Result};

/// Canonical text length of a hyphenated UUID.
pub const UUID_TEXT_LEN: usize = 36;

/// Parse a UUID that must already be in canonical lowercase hyphenated text form and carry an
/// accepted RFC version (4 random, 5 name-based, 7 time-ordered).
pub fn parse_canonical_uuid(kind: &str, s: &str) -> Result<Uuid> {
    if s.len() != UUID_TEXT_LEN {
        refuse!("invalid {kind}: {s:?} is not a canonical 36-character UUID");
    }
    let u =
        Uuid::try_parse(s).map_err(|e| ProtocolError::new(format!("invalid {kind} {s:?}: {e}")))?;
    if u.hyphenated().to_string() != s {
        refuse!("invalid {kind}: {s:?} is not canonical lowercase hyphenated UUID text");
    }
    check_uuid(kind, u)?;
    Ok(u)
}

/// Structural rules shared by parsing and construction.
pub fn check_uuid(kind: &str, u: Uuid) -> Result<()> {
    if u.is_nil() || u.as_bytes() == &[0xff; 16] {
        refuse!("invalid {kind}: nil/max UUID is not an identity");
    }
    if u.get_variant() != Variant::RFC4122 {
        refuse!("invalid {kind}: {u} is not an RFC 4122/9562 variant UUID");
    }
    match u.get_version_num() {
        4 | 5 | 7 => Ok(()),
        v => refuse!("invalid {kind}: {u} has unsupported UUID version {v} (need 4, 5 or 7)"),
    }
}

macro_rules! sync_id {
    ($(#[$doc:meta])* $name:ident, $kind:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            pub const KIND: &'static str = $kind;

            /// Wrap an existing UUID after structural validation.
            pub fn from_uuid(u: Uuid) -> Result<Self> {
                check_uuid(Self::KIND, u)?;
                Ok(Self(u))
            }

            /// Strict canonical parse.
            pub fn parse(s: &str) -> Result<Self> {
                parse_canonical_uuid(Self::KIND, s).map(Self)
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }

            /// Mint a new time-ordered UUIDv7 identity.
            #[cfg(feature = "mint")]
            pub fn mint() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.hyphenated(), f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = ProtocolError;

            fn from_str(s: &str) -> Result<Self> {
                Self::parse(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                struct V;
                impl Visitor<'_> for V {
                    type Value = $name;

                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, "a canonical {} UUID string", $kind)
                    }

                    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<$name, E> {
                        $name::parse(v).map_err(E::custom)
                    }
                }
                d.deserialize_str(V)
            }
        }
    };
}

sync_id!(
    /// One shared notebook library (the replication unit).
    LibraryId,
    "LibraryId"
);
sync_id!(
    /// A notebook, independent of its local directory name, title, or position.
    NotebookId,
    "NotebookId"
);
sync_id!(
    /// A page, independent of its local directory name or position.
    PageId,
    "PageId"
);
sync_id!(
    /// A visible stroke or erase fragment (and, as `root_id`, an authored lineage root).
    StrokeId,
    "StrokeId"
);
sync_id!(
    /// A positioned page text object (peer of strokes; never a stroke ID).
    TextId,
    "TextId"
);
sync_id!(
    /// An explicit spatial page-to-page link (never derived from handwriting or names).
    LinkId,
    "LinkId"
);
sync_id!(
    /// A library folder grouping notebooks (display only; notebooks keep their own identity).
    FolderId,
    "FolderId"
);
sync_id!(
    /// One installation/client (Kindle, browser profile, ...). Never library content.
    ReplicaId,
    "ReplicaId"
);
sync_id!(
    /// One immutable semantic operation; the future server deduplicates on it.
    OperationId,
    "OperationId"
);

#[cfg(test)]
mod tests {
    use super::*;

    const V7: &str = "019a8ef2-3c4d-7abc-8def-0123456789ab";

    #[test]
    fn canonical_parse_format_and_serde_round_trip() {
        let id = NotebookId::parse(V7).unwrap();
        assert_eq!(id.to_string(), V7);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{V7}\""));
        let back: NotebookId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
        assert_eq!(V7.parse::<NotebookId>().unwrap(), id);
        let mut set = std::collections::HashSet::new();
        assert!(set.insert(id));
        assert!(!set.insert(back));
    }

    #[test]
    fn malformed_values_are_refused() {
        for bad in [
            "",
            "not-a-uuid",
            "019A8EF2-3C4D-7ABC-8DEF-0123456789AB", // uppercase
            "{019a8ef2-3c4d-7abc-8def-0123456789ab}", // braced
            "019a8ef23c4d7abc8def0123456789ab",     // simple
            "urn:uuid:019a8ef2-3c4d-7abc-8def-0123456789ab",
            "00000000-0000-0000-0000-000000000000", // nil
            "ffffffff-ffff-ffff-ffff-ffffffffffff", // max
            "019a8ef2-3c4d-1abc-8def-0123456789ab", // v1
            "019a8ef2-3c4d-7abc-0def-0123456789ab", // NCS variant
            " 019a8ef2-3c4d-7abc-8def-0123456789ab",
        ] {
            assert!(LibraryId::parse(bad).is_err(), "{bad:?}");
            assert!(serde_json::from_str::<PageId>(&format!("{bad:?}")).is_err());
        }
        assert!(serde_json::from_str::<PageId>("42").is_err());
    }

    #[test]
    fn typed_ids_do_not_mix() {
        // Compile-time: a NotebookId cannot be passed where a PageId is expected. Runtime: the
        // same UUID text parses into either type only by explicit choice.
        let n = NotebookId::parse(V7).unwrap();
        let p = PageId::parse(V7).unwrap();
        assert_eq!(n.as_uuid(), p.as_uuid());
        assert!(format!("{n:?}").starts_with("NotebookId("));
    }

    #[cfg(feature = "mint")]
    #[test]
    fn minted_ids_are_v7_and_unique() {
        let ids: std::collections::HashSet<_> = (0..10_000).map(|_| OperationId::mint()).collect();
        assert_eq!(ids.len(), 10_000);
        for id in ids.iter().take(10) {
            assert_eq!(id.as_uuid().get_version_num(), 7);
            assert_eq!(OperationId::parse(&id.to_string()).unwrap(), *id);
        }
    }
}
