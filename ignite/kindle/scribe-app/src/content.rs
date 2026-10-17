//! Cross-content identity, canonical bounds, and point hit-testing.
//!
//! Addresses only. These types do not persist, sync, render, or mutate. The Kindle's
//! stroke-lineage lasso policy lives separately in `lasso.rs`; this module deliberately does
//! not turn point hit-testing into a generic region-selection abstraction.

use notebook_core::{hit_test_topmost_stroke, Point, StrokeId, PANEL_TO_CANONICAL_SCALE};

use crate::link_paint;
use crate::open_page::{BoundPageKey, OpenPage};
use crate::profile::LEGACY_FIXED_RADIUS;
use crate::text_paint::{point_in_text_layout, text_layout_height};
use sync_protocol::{LinkId, TextId};

/// Point-hit tolerance for strokes, in canonical page units.
///
/// Equal to the sealed legacy fixed brush radius (2 panel px) mapped through the
/// Scribe display scale (8.4). This is **not** the eraser capsule (240) and **not**
/// an exact visual-ink AABB: pressure-width strokes can paint wider or narrower,
/// and [`notebook_core::Stroke::bounds`] is a point AABB with no brush radius.
pub const CONTENT_STROKE_HIT_RADIUS_CANONICAL: f32 =
    (LEGACY_FIXED_RADIUS as f32) * PANEL_TO_CANONICAL_SCALE;

/// Address of one durable object on the bound page.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ContentRef {
    /// Visible stroke or erase fragment (`PageSession` id), not automatically the lineage root.
    Stroke(StrokeId),
    Text(TextId),
    /// Compact link icon at the stored canonical point (not handwriting).
    Link(LinkId),
}

/// Axis-aligned box in canonical page coordinates (origin top-left, +Y down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CanonicalRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl CanonicalRect {
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x && p.x < self.x + self.width && p.y >= self.y && p.y < self.y + self.height
    }
}

/// Replica-local, transient selection. Not persisted, not synced, not painted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentSelection {
    pub bound: BoundPageKey,
    pub refs: Vec<ContentRef>,
}

impl ContentSelection {
    pub fn new(bound: BoundPageKey, refs: Vec<ContentRef>) -> Self {
        Self { bound, refs }
    }

    pub fn empty(bound: BoundPageKey) -> Self {
        Self::new(bound, Vec::new())
    }
}

/// Canonical bounds of a live object. Missing/deleted ids return `None`.
///
/// Stroke boxes are **point AABBs** ([`notebook_core::Stroke::bounds`]): they do
/// not include brush/pressure width. Text boxes are the Kindle layout box
/// (origin + width + wrap-derived height), not glyph ink.
pub fn bounds(page: &OpenPage<'_>, r: &ContentRef) -> Option<CanonicalRect> {
    match r {
        ContentRef::Stroke(id) => {
            let (min_x, min_y, max_x, max_y) = page.stroke(id)?.bounds()?;
            Some(CanonicalRect {
                x: min_x,
                y: min_y,
                width: max_x - min_x,
                height: max_y - min_y,
            })
        }
        ContentRef::Text(id) => {
            let t = page.text(*id)?;
            Some(CanonicalRect {
                x: t.x,
                y: t.y,
                width: t.width,
                height: text_layout_height(t),
            })
        }
        ContentRef::Link(id) => page.link(*id).map(link_paint::icon_canonical_rect),
    }
}

/// Topmost content at a canonical page point.
///
/// Paint order is paper → strokes → text → link icons, so hit order is:
/// 1. link icons, newest/topmost first;
/// 2. texts, newest/topmost first (later list index wins);
/// 3. strokes, newest/topmost first ([`hit_test_topmost_stroke`]).
///
/// Does not inspect framebuffer pixels or overlay coverage.
pub fn hit_test(page: &OpenPage<'_>, p: Point) -> Option<ContentRef> {
    for l in page.links().iter().rev() {
        if link_paint::contains_point(l, p.x, p.y) {
            return Some(ContentRef::Link(l.link_id));
        }
    }
    for t in page.texts().iter().rev() {
        if point_in_text_layout(t, p.x, p.y) {
            return Some(ContentRef::Text(t.text_id));
        }
    }
    hit_test_topmost_stroke(
        page.session().committed_strokes(),
        p,
        CONTENT_STROKE_HIT_RADIUS_CANONICAL,
    )
    .cloned()
    .map(ContentRef::Stroke)
}

/// Drop refs that are not live on `page`. A selection for a different bound page
/// (including the same local `page-N` name in another notebook) resolves to no
/// refs — it does not pick up identical-looking ids elsewhere.
pub fn resolve_selection(page: &OpenPage<'_>, sel: &ContentSelection) -> ContentSelection {
    if sel.bound != *page.bound() {
        return ContentSelection {
            bound: sel.bound.clone(),
            refs: Vec::new(),
        };
    }
    ContentSelection {
        bound: sel.bound.clone(),
        refs: sel
            .refs
            .iter()
            .filter(|r| match r {
                ContentRef::Stroke(id) => page.contains_stroke(id),
                ContentRef::Text(id) => page.contains_text(*id),
                ContentRef::Link(id) => page.contains_link(*id),
            })
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::PageSession;
    use notebook_core::{Point, Stroke, StrokeId};
    use sync_engine::testkit;

    fn stroke(id: &str, pts: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: pts.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            brush: None,
            finished: true,
        }
    }

    fn key(page: &str) -> BoundPageKey {
        BoundPageKey::new("notebook-1", page)
    }

    fn page(strokes: Vec<Stroke>) -> PageSession {
        let mut s = PageSession::new("page-1");
        if !strokes.is_empty() {
            s.load_committed_strokes(strokes).unwrap();
        }
        s
    }

    #[test]
    fn text_over_stroke_wins() {
        let bound = key("page-1");
        let mut text = testkit::text_obj(1, "Hello");
        text.x = 100.0;
        text.y = 200.0;
        text.width = 5000.0;
        let s = stroke("s-under", &[(100.0, 220.0), (800.0, 220.0)]);
        let session = page(vec![s]);
        let open = OpenPage::new(&bound, &session, std::slice::from_ref(&text));
        let hit = hit_test(&open, Point::new(200.0, 220.0)).unwrap();
        assert_eq!(hit, ContentRef::Text(text.text_id));
    }

    #[test]
    fn link_icon_wins_over_text_and_stroke() {
        let bound = key("page-1");
        let mut text = testkit::text_obj(1, "Hello");
        text.x = 1900.0;
        text.y = 2900.0;
        text.width = 5000.0;
        let s = stroke("s-under", &[(2000.0, 3000.0), (2500.0, 3000.0)]);
        let session = page(vec![s]);
        let link = testkit::page_link(
            1,
            testkit::nb(1),
            testkit::pg(1),
            testkit::nb(2),
            testkit::pg(2),
        );
        let open = OpenPage::with_links(
            &bound,
            &session,
            std::slice::from_ref(&text),
            std::slice::from_ref(&link),
        );
        let hit = hit_test(&open, Point::new(link.source_x, link.source_y)).unwrap();
        assert_eq!(hit, ContentRef::Link(link.link_id));
    }

    #[test]
    fn stroke_without_text_overlap_wins() {
        let bound = key("page-1");
        let text = testkit::text_obj(1, "Hello");
        let s = stroke("s-far", &[(100.0, 10000.0), (800.0, 10000.0)]);
        let session = page(vec![s]);
        let open = OpenPage::new(&bound, &session, std::slice::from_ref(&text));
        let hit = hit_test(&open, Point::new(200.0, 10000.0)).unwrap();
        assert_eq!(hit, ContentRef::Stroke(StrokeId::new("s-far")));
    }

    #[test]
    fn text_only_point() {
        let bound = key("page-1");
        let text = testkit::text_obj(1, "Hello");
        let session = page(vec![]);
        let open = OpenPage::new(&bound, &session, std::slice::from_ref(&text));
        let hit = hit_test(&open, Point::new(text.x + 10.0, text.y + 10.0)).unwrap();
        assert_eq!(hit, ContentRef::Text(text.text_id));
    }

    #[test]
    fn empty_point_is_none() {
        let bound = key("page-1");
        let text = testkit::text_obj(1, "Hello");
        let s = stroke("s-a", &[(100.0, 10000.0), (200.0, 10000.0)]);
        let session = page(vec![s]);
        let open = OpenPage::new(&bound, &session, std::slice::from_ref(&text));
        assert_eq!(hit_test(&open, Point::new(50.0, 50.0)), None);
    }

    #[test]
    fn missing_ref_has_no_bounds() {
        let bound = key("page-1");
        let session = PageSession::new("page-1");
        let open = OpenPage::new(&bound, &session, &[]);
        assert_eq!(
            bounds(&open, &ContentRef::Stroke(StrokeId::new("s-gone"))),
            None
        );
        assert_eq!(bounds(&open, &ContentRef::Text(testkit::tx(99))), None);
        assert_eq!(bounds(&open, &ContentRef::Link(testkit::lk(99))), None);
    }

    #[test]
    fn later_text_is_topmost() {
        let bound = key("page-1");
        let a = testkit::text_obj(1, "A");
        let mut b = testkit::text_obj(2, "B");
        b.x = a.x;
        b.y = a.y;
        b.width = a.width;
        let session = page(vec![]);
        let texts = [a, b.clone()];
        let open = OpenPage::new(&bound, &session, &texts);
        let hit = hit_test(&open, Point::new(b.x + 1.0, b.y + 1.0)).unwrap();
        assert_eq!(hit, ContentRef::Text(b.text_id));
    }

    #[test]
    fn stroke_bounds_are_point_aabb() {
        let bound = key("page-1");
        let s = stroke("s-a", &[(10.0, 20.0), (40.0, 80.0)]);
        let session = page(vec![s]);
        let open = OpenPage::new(&bound, &session, &[]);
        let r = bounds(&open, &ContentRef::Stroke(StrokeId::new("s-a"))).unwrap();
        assert_eq!(r.x, 10.0);
        assert_eq!(r.y, 20.0);
        assert_eq!(r.width, 30.0);
        assert_eq!(r.height, 60.0);
        assert!(r.contains(Point::new(10.0, 20.0)));
        assert!(!r.contains(Point::new(9.0, 20.0)));
    }

    #[test]
    fn deleted_text_drops_from_selection() {
        let bound = key("page-1");
        let live = testkit::text_obj(1, "keep");
        let gone = testkit::tx(2);
        let session = page(vec![]);
        let open = OpenPage::new(&bound, &session, std::slice::from_ref(&live));
        let sel = ContentSelection::new(
            bound.clone(),
            vec![ContentRef::Text(live.text_id), ContentRef::Text(gone)],
        );
        let out = resolve_selection(&open, &sel);
        assert_eq!(out.refs, vec![ContentRef::Text(live.text_id)]);
    }

    #[test]
    fn stale_stroke_fragment_drops_without_retargeting_root() {
        let bound = key("page-1");
        let visible = stroke("s-frag", &[(0.0, 0.0), (10.0, 0.0)]);
        let mut session = PageSession::new("page-1");
        session
            .load_committed_strokes_with_roots(
                vec![visible],
                [("s-frag".into(), "s-root".into())].into_iter().collect(),
            )
            .unwrap();
        let open = OpenPage::new(&bound, &session, &[]);
        let sel = ContentSelection::new(
            bound.clone(),
            vec![
                ContentRef::Stroke(StrokeId::new("s-frag")),
                ContentRef::Stroke(StrokeId::new("s-root")),
            ],
        );
        let out = resolve_selection(&open, &sel);
        assert_eq!(out.refs, vec![ContentRef::Stroke(StrokeId::new("s-frag"))]);
    }

    #[test]
    fn page_switch_does_not_resolve_foreign_ids() {
        let bound = key("page-2");
        let s = stroke("s-shared", &[(0.0, 0.0), (10.0, 0.0)]);
        let mut session = PageSession::new("page-2");
        session.load_committed_strokes(vec![s]).unwrap();
        let open = OpenPage::new(&bound, &session, &[]);
        let sel = ContentSelection::new(
            key("page-1"),
            vec![ContentRef::Stroke(StrokeId::new("s-shared"))],
        );
        let out = resolve_selection(&open, &sel);
        assert_eq!(out.bound, key("page-1"));
        assert!(out.refs.is_empty());
    }

    #[test]
    fn same_page_name_in_another_notebook_does_not_resolve() {
        let a = BoundPageKey::new("notebook-a", "page-1");
        let b = BoundPageKey::new("notebook-b", "page-1");
        let s = stroke("s-shared", &[(0.0, 0.0), (10.0, 0.0)]);
        let session = page(vec![s]);
        let open = OpenPage::new(&b, &session, &[]);
        let sel = ContentSelection::new(
            a.clone(),
            vec![ContentRef::Stroke(StrokeId::new("s-shared"))],
        );
        let out = resolve_selection(&open, &sel);
        assert_eq!(out.bound, a);
        assert!(
            out.refs.is_empty(),
            "page-1 in notebook-a must not resolve against page-1 in notebook-b"
        );
    }

    #[test]
    fn dropping_one_stale_ref_keeps_the_rest() {
        let bound = key("page-1");
        let keep_stroke = stroke("s-keep", &[(0.0, 0.0), (5.0, 0.0)]);
        let keep_text = testkit::text_obj(1, "ok");
        let session = page(vec![keep_stroke]);
        let open = OpenPage::new(&bound, &session, std::slice::from_ref(&keep_text));
        let sel = ContentSelection::new(
            bound.clone(),
            vec![
                ContentRef::Stroke(StrokeId::new("s-keep")),
                ContentRef::Stroke(StrokeId::new("s-gone")),
                ContentRef::Text(keep_text.text_id),
            ],
        );
        let out = resolve_selection(&open, &sel);
        assert_eq!(
            out.refs,
            vec![
                ContentRef::Stroke(StrokeId::new("s-keep")),
                ContentRef::Text(keep_text.text_id),
            ]
        );
    }
}
