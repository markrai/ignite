//! Local stroke ID ↔ global [`StrokeId`] compatibility rule.
//!
//! Local stroke IDs are opaque strings persisted in `strokes.jsonl` / `edits.jsonl`. They are
//! never rewritten. Two families exist:
//!
//! ```text
//! UUID-backed (post-S1)  "s-<canonical UUIDv7 or UUIDv4>"   e.g. s-019a8ef2-3c4d-7abc-8def-0123456789ab
//!                        → global StrokeId = the embedded UUID (page-independent)
//! legacy (pre-S1)        anything else, e.g. "s-1727000000000-12"
//!                        → global StrokeId = UUIDv5(namespace = PageId, name = exact local ID bytes)
//! ```
//!
//! Embedded UUIDs must be version 4 or 7, while legacy derivations are always version 5, so the
//! two families can never collide. The same legacy string on two pages maps to two different
//! global IDs; the same (page, legacy string) maps to the same global ID forever.

use uuid::Uuid;

use crate::error::{refuse, Result};
use crate::ids::{parse_canonical_uuid, PageId, StrokeId, UUID_TEXT_LEN};

pub const LOCAL_STROKE_PREFIX: &str = "s-";
/// Longest accepted local stroke ID (legacy IDs are ~20 bytes; UUID-backed are 38).
pub const MAX_LOCAL_STROKE_ID_LEN: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalStrokeIdKind {
    /// `s-<uuid v4/v7>`: the embedded UUID is the global identity.
    UuidBacked(StrokeId),
    /// Pre-S1 local-only allocation; global identity is derived per page.
    Legacy,
}

fn check_local(local: &str) -> Result<()> {
    if local.is_empty() {
        refuse!("invalid local stroke id: empty");
    }
    if local.len() > MAX_LOCAL_STROKE_ID_LEN {
        refuse!("invalid local stroke id: longer than {MAX_LOCAL_STROKE_ID_LEN} bytes");
    }
    if local.chars().any(char::is_control) {
        refuse!("invalid local stroke id {local:?}: control characters");
    }
    Ok(())
}

/// Classify a persisted local stroke ID.
pub fn classify_local_stroke_id(local: &str) -> Result<LocalStrokeIdKind> {
    check_local(local)?;
    if let Some(rest) = local.strip_prefix(LOCAL_STROKE_PREFIX) {
        if rest.len() == UUID_TEXT_LEN {
            if let Ok(u) = parse_canonical_uuid("StrokeId", rest) {
                if matches!(u.get_version_num(), 4 | 7) {
                    return Ok(LocalStrokeIdKind::UuidBacked(StrokeId::from_uuid(u)?));
                }
            }
        }
    }
    Ok(LocalStrokeIdKind::Legacy)
}

/// Global identity of a local stroke/fragment/root ID on `page`.
pub fn global_stroke_id(page: PageId, local: &str) -> Result<StrokeId> {
    match classify_local_stroke_id(local)? {
        LocalStrokeIdKind::UuidBacked(id) => Ok(id),
        LocalStrokeIdKind::Legacy => {
            StrokeId::from_uuid(Uuid::new_v5(&page.as_uuid(), local.as_bytes()))
        }
    }
}

/// Local persisted form of a UUID-backed stroke ID (what the Kindle allocator writes).
pub fn uuid_backed_local_stroke_id(id: StrokeId) -> String {
    format!("{LOCAL_STROKE_PREFIX}{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(s: &str) -> PageId {
        PageId::parse(s).unwrap()
    }

    const PAGE_A: &str = "019a8ef2-0000-7000-8000-00000000000a";
    const PAGE_B: &str = "019a8ef2-0000-7000-8000-00000000000b";

    #[test]
    fn legacy_ids_map_deterministically_and_page_scoped() {
        let a = page(PAGE_A);
        let b = page(PAGE_B);
        let x = global_stroke_id(a, "s-123-4").unwrap();
        assert_eq!(global_stroke_id(a, "s-123-4").unwrap(), x, "stable forever");
        let y = global_stroke_id(b, "s-123-4").unwrap();
        assert_ne!(x, y, "same local string on another page is another stroke");
        assert_eq!(x.as_uuid().get_version_num(), 5);
        // Pinned value: changing the derivation would silently re-identify every legacy stroke.
        assert_eq!(x.as_uuid(), Uuid::new_v5(&a.as_uuid(), b"s-123-4"));
        assert_eq!(
            classify_local_stroke_id("s-1727000000000-12").unwrap(),
            LocalStrokeIdKind::Legacy
        );
    }

    #[test]
    fn uuid_backed_ids_preserve_the_embedded_uuid_on_any_page() {
        let embedded = "019a8ef2-3c4d-7abc-8def-0123456789ab";
        let local = format!("s-{embedded}");
        let want = StrokeId::parse(embedded).unwrap();
        assert_eq!(global_stroke_id(page(PAGE_A), &local).unwrap(), want);
        assert_eq!(global_stroke_id(page(PAGE_B), &local).unwrap(), want);
        assert_eq!(uuid_backed_local_stroke_id(want), local);
    }

    #[test]
    fn near_misses_are_legacy_not_uuid_backed() {
        for local in [
            "s-019A8EF2-3C4D-7ABC-8DEF-0123456789AB", // uppercase
            "s-019a8ef2-3c4d-5abc-8def-0123456789ab", // v5 must never be embedded
            "x-019a8ef2-3c4d-7abc-8def-0123456789ab",
            "s-019a8ef2-3c4d-7abc-8def-0123456789ab-extra",
        ] {
            assert_eq!(
                classify_local_stroke_id(local).unwrap(),
                LocalStrokeIdKind::Legacy,
                "{local}"
            );
        }
        assert!(classify_local_stroke_id("").is_err());
        assert!(classify_local_stroke_id("s-\n1").is_err());
    }
}
