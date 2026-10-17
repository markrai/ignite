//! Stable identifiers for sync-friendly notebook entities.

use std::fmt;

/// Opaque stroke identifier. Stable once a stroke is finished.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StrokeId(String);

impl StrokeId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StrokeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Allocate a new globally unique stroke id.
///
/// Format (Sync S1): `s-<UUIDv7>` in canonical lowercase hyphenated form, e.g.
/// `s-019a8ef2-3c4d-7abc-8def-0123456789ab`. The embedded UUID *is* the stroke's global sync
/// identity (see `sync-protocol::stroke_ids`). Pre-S1 IDs (`s-{unix_millis}-{seq}`) remain valid
/// forever and are never rewritten; their global identity is derived per page instead.
///
/// Requires feature `mint` (default).
///
/// Cost: one UUIDv7 (timestamp + OS randomness), no I/O beyond the RNG — the only S1 change on
/// the pen hot path.
#[cfg(feature = "mint")]
pub fn new_stroke_id() -> StrokeId {
    StrokeId::new(format!("s-{}", uuid::Uuid::now_v7().hyphenated()))
}

#[cfg(all(test, feature = "mint"))]
mod tests {
    use super::*;

    #[test]
    fn stroke_ids_are_distinct() {
        let a = new_stroke_id();
        let b = new_stroke_id();
        assert_ne!(a, b);
    }

    #[test]
    fn new_stroke_ids_embed_canonical_uuid_v7() {
        let ids: std::collections::HashSet<String> = (0..50_000)
            .map(|_| new_stroke_id().as_str().to_string())
            .collect();
        assert_eq!(ids.len(), 50_000, "all distinct");
        for id in ids.iter().take(100) {
            let rest = id.strip_prefix("s-").expect("s- prefix");
            let u = uuid::Uuid::try_parse(rest).expect("uuid");
            assert_eq!(u.get_version_num(), 7);
            assert_eq!(u.hyphenated().to_string(), rest, "canonical lowercase");
        }
    }
}
