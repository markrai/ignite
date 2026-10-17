//! Finger-lasso geometry and replica-local selection state.
//!
//! This module is deliberately page-local and host-testable. It never paints, persists, or
//! emits sync operations. A completed drag is returned as one canonical lineage translation;
//! the runtime owns the durable write-ahead/commit boundary.

use std::collections::BTreeSet;

use kindle_platform::DirtyRegion;
use notebook_core::{Point, Stroke, CANONICAL_HEIGHT, CANONICAL_WIDTH};

use crate::content::{CanonicalRect, CONTENT_STROKE_HIT_RADIUS_CANONICAL};
use crate::open_page::{BoundPageKey, OpenPage};
use crate::store::{PersistedFragmentV2, PersistedPointV1, PersistedReplacementV2};

/// Touch samples closer than two panel pixels at the native 8.4:1 scale are coalesced.
pub const LASSO_SAMPLE_MIN_DISTANCE_CANONICAL: f32 = 2.0 * 8.4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectedObject {
    /// Authored lineage root (local page-journal identity), including all visible fragments.
    StrokeLineage(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LassoSelection {
    pub bound: BoundPageKey,
    pub objects: Vec<SelectedObject>,
    pub bounds: CanonicalRect,
}

impl LassoSelection {
    pub fn roots(&self) -> impl Iterator<Item = &str> {
        self.objects.iter().map(|o| match o {
            SelectedObject::StrokeLineage(root) => root.as_str(),
        })
    }

    pub fn contains(&self, point: Point) -> bool {
        self.bounds.contains(point)
    }

    pub fn translated_bounds(&self, dx: f32, dy: f32) -> CanonicalRect {
        CanonicalRect {
            x: self.bounds.x + dx,
            y: self.bounds.y + dy,
            ..self.bounds
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MoveCommit {
    pub bound: BoundPageKey,
    pub roots: Vec<String>,
    pub dx: f32,
    pub dy: f32,
    pub old_bounds: CanonicalRect,
    pub new_bounds: CanonicalRect,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum LassoState {
    #[default]
    Idle,
    LassoDrawing {
        bound: BoundPageKey,
        points: Vec<Point>,
    },
    Selected(LassoSelection),
    Moving {
        selection: LassoSelection,
        start: Point,
        current: Point,
        dx: f32,
        dy: f32,
    },
}

impl LassoState {
    pub fn selection(&self) -> Option<&LassoSelection> {
        match self {
            Self::Selected(s) | Self::Moving { selection: s, .. } => Some(s),
            Self::Idle | Self::LassoDrawing { .. } => None,
        }
    }

    pub fn preview_delta(&self) -> (f32, f32) {
        match self {
            Self::Moving { dx, dy, .. } => (*dx, *dy),
            _ => (0.0, 0.0),
        }
    }

    pub fn drawing_points(&self) -> Option<&[Point]> {
        match self {
            Self::LassoDrawing { points, .. } => Some(points),
            _ => None,
        }
    }

    pub fn clear(&mut self) {
        *self = Self::Idle;
    }

    pub fn select_roots(&mut self, bound: BoundPageKey, roots: Vec<String>, bounds: CanonicalRect) {
        *self = Self::Selected(LassoSelection {
            bound,
            objects: roots
                .into_iter()
                .map(SelectedObject::StrokeLineage)
                .collect(),
            bounds,
        });
    }

    pub fn begin_lasso(&mut self, bound: BoundPageKey, point: Point) {
        *self = Self::LassoDrawing {
            bound,
            points: vec![point],
        };
    }

    /// Append one canonical point if it is far enough from the last retained sample.
    pub fn push_lasso_point(&mut self, point: Point) -> bool {
        let Self::LassoDrawing { points, .. } = self else {
            return false;
        };
        let Some(last) = points.last() else {
            points.push(point);
            return true;
        };
        if distance_sq(*last, point)
            < LASSO_SAMPLE_MIN_DISTANCE_CANONICAL * LASSO_SAMPLE_MIN_DISTANCE_CANONICAL
        {
            return false;
        }
        points.push(point);
        true
    }

    /// Close the polygon implicitly and resolve stroke lineages on the currently bound page.
    pub fn finish_lasso(&mut self, page: &OpenPage<'_>) -> Option<&LassoSelection> {
        let ended = std::mem::take(self);
        let Self::LassoDrawing { bound, points } = ended else {
            return None;
        };
        if bound != *page.bound() {
            return None;
        }
        let selection =
            select_stroke_lineages(page, &points).map(|(objects, bounds)| LassoSelection {
                bound,
                objects,
                bounds,
            })?;
        *self = Self::Selected(selection);
        self.selection()
    }

    pub fn begin_move(&mut self, point: Point) -> bool {
        let ended = std::mem::take(self);
        match ended {
            Self::Selected(selection) if selection.contains(point) => {
                *self = Self::Moving {
                    selection,
                    start: point,
                    current: point,
                    dx: 0.0,
                    dy: 0.0,
                };
                true
            }
            other => {
                *self = other;
                false
            }
        }
    }

    /// Update and page-clamp the canonical preview translation.
    pub fn update_move(&mut self, point: Point) -> Option<(f32, f32)> {
        let Self::Moving {
            selection,
            start,
            current,
            dx,
            dy,
        } = self
        else {
            return None;
        };
        *current = point;
        let wanted_x = point.x - start.x;
        let wanted_y = point.y - start.y;
        let min_dx = -selection.bounds.x;
        let max_dx = CANONICAL_WIDTH - (selection.bounds.x + selection.bounds.width);
        let min_dy = -selection.bounds.y;
        let max_dy = CANONICAL_HEIGHT - (selection.bounds.y + selection.bounds.height);
        *dx = wanted_x.clamp(min_dx, max_dx);
        *dy = wanted_y.clamp(min_dy, max_dy);
        Some((*dx, *dy))
    }

    pub fn cancel_move(&mut self) {
        let ended = std::mem::take(self);
        if let Self::Moving { selection, .. } = ended {
            *self = Self::Selected(selection);
        }
    }

    /// Finish a drag. Zero-distance gestures keep the selection but emit no mutation.
    pub fn finish_move(&mut self) -> Option<MoveCommit> {
        let ended = std::mem::take(self);
        let Self::Moving {
            mut selection,
            dx,
            dy,
            ..
        } = ended
        else {
            return None;
        };
        if dx == 0.0 && dy == 0.0 {
            *self = Self::Selected(selection);
            return None;
        }
        let old_bounds = selection.bounds;
        let new_bounds = selection.translated_bounds(dx, dy);
        let commit = MoveCommit {
            bound: selection.bound.clone(),
            roots: selection.roots().map(str::to_owned).collect(),
            dx,
            dy,
            old_bounds,
            new_bounds,
        };
        selection.bounds = new_bounds;
        *self = Self::Selected(selection);
        Some(commit)
    }
}

fn distance_sq(a: Point, b: Point) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx * dx + dy * dy
}

fn rect_intersects(a: CanonicalRect, b: CanonicalRect) -> bool {
    a.x <= b.x + b.width && b.x <= a.x + a.width && a.y <= b.y + b.height && b.y <= a.y + a.height
}

/// Replica-local overlay footprints. Lasso-outline pixels accumulate until the outline is
/// removed; a selection-move preview stores only the **current** painted box, not the drag path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchOverlayDirty {
    pub lasso_outline: Option<DirtyRegion>,
    pub move_preview: Option<DirtyRegion>,
}

impl TouchOverlayDirty {
    pub fn is_some(self) -> bool {
        self.lasso_outline.is_some() || self.move_preview.is_some()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Union of every transient overlay still on the panel, then forget both.
    pub fn take_cleanup(&mut self) -> Option<DirtyRegion> {
        match (self.lasso_outline.take(), self.move_preview.take()) {
            (None, None) => None,
            (Some(a), None) | (None, Some(a)) => Some(a),
            (Some(a), Some(b)) => Some(a.union(b)),
        }
    }

    /// Path accumulation is correct for the freeform outline: earlier segments stay visible.
    pub fn add_lasso_segment(&mut self, segment: DirtyRegion) {
        self.lasso_outline = Some(match self.lasso_outline {
            Some(existing) => existing.union(segment),
            None => segment,
        });
    }

    /// Restore previous preview (and any leftover outline) then paint `current`.
    /// After the paint, the stored footprint is `current` — not the union.
    pub fn begin_move_preview(&mut self, current: DirtyRegion) -> DirtyRegion {
        let restore = self.take_cleanup();
        let paint = restore.map_or(current, |old| old.union(current));
        self.move_preview = Some(current);
        paint
    }
}

/// Canonical repaint extent for a completed move. The runtime adds panel/render padding after
/// the display transform; keeping this union in page space makes the old and new geometry
/// explicit and testable.
pub fn move_dirty_bounds(old: CanonicalRect, new: CanonicalRect) -> CanonicalRect {
    let x = old.x.min(new.x);
    let y = old.y.min(new.y);
    let right = (old.x + old.width).max(new.x + new.width);
    let bottom = (old.y + old.height).max(new.y + new.height);
    CanonicalRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

fn polygon_bounds(points: &[Point]) -> Option<CanonicalRect> {
    let first = *points.first()?;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (first.x, first.y, first.x, first.y);
    for p in &points[1..] {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    Some(CanonicalRect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    })
}

fn padded(mut r: CanonicalRect, amount: f32) -> CanonicalRect {
    let x0 = (r.x - amount).max(0.0);
    let y0 = (r.y - amount).max(0.0);
    let x1 = (r.x + r.width + amount).min(CANONICAL_WIDTH);
    let y1 = (r.y + r.height + amount).min(CANONICAL_HEIGHT);
    r.x = x0;
    r.y = y0;
    r.width = x1 - x0;
    r.height = y1 - y0;
    r
}

fn point_in_polygon(point: Point, polygon: &[Point]) -> bool {
    let mut inside = false;
    let mut j = polygon.len() - 1;
    for i in 0..polygon.len() {
        let a = polygon[i];
        let b = polygon[j];
        if ((a.y > point.y) != (b.y > point.y))
            && point.x < (b.x - a.x) * (point.y - a.y) / (b.y - a.y) + a.x
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn orientation(a: Point, b: Point, c: Point) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn on_segment(a: Point, b: Point, p: Point) -> bool {
    p.x >= a.x.min(b.x) && p.x <= a.x.max(b.x) && p.y >= a.y.min(b.y) && p.y <= a.y.max(b.y)
}

fn segments_intersect(a: Point, b: Point, c: Point, d: Point) -> bool {
    let (o1, o2, o3, o4) = (
        orientation(a, b, c),
        orientation(a, b, d),
        orientation(c, d, a),
        orientation(c, d, b),
    );
    if ((o1 > 0.0 && o2 < 0.0) || (o1 < 0.0 && o2 > 0.0))
        && ((o3 > 0.0 && o4 < 0.0) || (o3 < 0.0 && o4 > 0.0))
    {
        return true;
    }
    const EPS: f32 = 0.001;
    (o1.abs() <= EPS && on_segment(a, b, c))
        || (o2.abs() <= EPS && on_segment(a, b, d))
        || (o3.abs() <= EPS && on_segment(c, d, a))
        || (o4.abs() <= EPS && on_segment(c, d, b))
}

fn point_segment_distance_sq(p: Point, a: Point, b: Point) -> f32 {
    let vx = b.x - a.x;
    let vy = b.y - a.y;
    let len = vx * vx + vy * vy;
    if len == 0.0 {
        return distance_sq(p, a);
    }
    let t = (((p.x - a.x) * vx + (p.y - a.y) * vy) / len).clamp(0.0, 1.0);
    distance_sq(p, Point::new(a.x + t * vx, a.y + t * vy))
}

fn segments_within(a: Point, b: Point, c: Point, d: Point, radius: f32) -> bool {
    segments_intersect(a, b, c, d)
        || [
            point_segment_distance_sq(a, c, d),
            point_segment_distance_sq(b, c, d),
            point_segment_distance_sq(c, a, b),
            point_segment_distance_sq(d, a, b),
        ]
        .into_iter()
        .any(|d| d <= radius * radius)
}

fn stroke_hits_polygon(stroke: &Stroke, polygon: &[Point], radius: f32) -> bool {
    if stroke.points.iter().any(|p| point_in_polygon(*p, polygon)) {
        return true;
    }
    let stroke_segments: Vec<(Point, Point)> = if stroke.points.len() == 1 {
        vec![(stroke.points[0], stroke.points[0])]
    } else {
        stroke.points.windows(2).map(|w| (w[0], w[1])).collect()
    };
    for (a, b) in stroke_segments {
        for i in 0..polygon.len() {
            let c = polygon[i];
            let d = polygon[(i + 1) % polygon.len()];
            if segments_within(a, b, c, d, radius) {
                return true;
            }
        }
    }
    false
}

/// Broad-phase AABB followed by point/segment-vs-closed-polygon tests. A hit fragment selects
/// its whole authored lineage; bounds cover every currently visible fragment of those roots.
pub fn select_stroke_lineages(
    page: &OpenPage<'_>,
    polygon: &[Point],
) -> Option<(Vec<SelectedObject>, CanonicalRect)> {
    if polygon.len() < 3 {
        return None;
    }
    let lasso_bounds = polygon_bounds(polygon)?;
    let broad = padded(lasso_bounds, CONTENT_STROKE_HIT_RADIUS_CANONICAL);
    let mut roots = BTreeSet::new();
    for stroke in page.session().committed_strokes() {
        let Some((x0, y0, x1, y1)) = stroke.bounds() else {
            continue;
        };
        let stroke_bounds = CanonicalRect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        };
        if !rect_intersects(broad, stroke_bounds) {
            continue;
        }
        if stroke_hits_polygon(stroke, polygon, CONTENT_STROKE_HIT_RADIUS_CANONICAL) {
            roots.insert(page.session().root_id_of(&stroke.id));
        }
    }
    if roots.is_empty() {
        return None;
    }
    let mut selected_bounds: Option<CanonicalRect> = None;
    for stroke in page.session().committed_strokes() {
        if !roots.contains(&page.session().root_id_of(&stroke.id)) {
            continue;
        }
        let Some((x0, y0, x1, y1)) = stroke.bounds() else {
            continue;
        };
        let b = CanonicalRect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        };
        selected_bounds = Some(match selected_bounds {
            None => b,
            Some(a) => {
                let nx = a.x.min(b.x);
                let ny = a.y.min(b.y);
                let rx = (a.x + a.width).max(b.x + b.width);
                let by = (a.y + a.height).max(b.y + b.height);
                CanonicalRect {
                    x: nx,
                    y: ny,
                    width: rx - nx,
                    height: by - ny,
                }
            }
        });
    }
    Some((
        roots
            .into_iter()
            .map(SelectedObject::StrokeLineage)
            .collect(),
        padded(selected_bounds?, CONTENT_STROKE_HIT_RADIUS_CANONICAL),
    ))
}

/// Build one atomic same-ID geometry rewrite per visible fragment of the selected roots.
pub fn move_replacements(
    page: &OpenPage<'_>,
    roots: &[String],
    dx: f32,
    dy: f32,
) -> Result<Vec<PersistedReplacementV2>, String> {
    let roots: BTreeSet<&str> = roots.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for stroke in page.session().committed_strokes() {
        if !roots.contains(page.session().root_id_of(&stroke.id).as_str()) {
            continue;
        }
        let mut points = Vec::with_capacity(stroke.points.len());
        for point in &stroke.points {
            let x = point.x + dx;
            let y = point.y + dy;
            crate::store::validate_persisted_canonical_xy(x, y)?;
            points.push(PersistedPointV1 {
                x,
                y,
                pressure: point.pressure,
            });
        }
        let id = stroke.id.as_str().to_string();
        out.push(PersistedReplacementV2 {
            target_stroke_id: id.clone(),
            fragments: vec![PersistedFragmentV2 {
                id,
                style: stroke.brush.clone(),
                points,
            }],
        });
    }
    if out.is_empty() {
        return Err("move targets no visible stroke fragments".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_page::BoundPageKey;
    use crate::orch::{FakeTransport, StrokeController};
    use crate::session::PageSession;
    use crate::store::{MemoryPageStore, PageStore};
    use kindle_platform::ClockDomain;
    use notebook_core::StrokeId;

    fn stroke(id: &str, points: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: points.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            brush: None,
            finished: true,
        }
    }

    fn square() -> Vec<Point> {
        vec![
            Point::new(100.0, 100.0),
            Point::new(300.0, 100.0),
            Point::new(300.0, 300.0),
            Point::new(100.0, 300.0),
        ]
    }

    #[test]
    fn inside_outside_and_crossing_follow_closed_polygon_geometry() {
        let mut session = PageSession::new("page-1");
        session
            .load_committed_strokes(vec![
                stroke("inside", &[(150.0, 150.0), (250.0, 250.0)]),
                stroke("outside", &[(500.0, 500.0), (600.0, 600.0)]),
                stroke("crossing", &[(50.0, 200.0), (350.0, 200.0)]),
            ])
            .unwrap();
        let bound = BoundPageKey::new("notebook-1", "page-1");
        let page = OpenPage::new(&bound, &session, &[]);
        let (objects, _) = select_stroke_lineages(&page, &square()).unwrap();
        assert_eq!(
            objects,
            vec![
                SelectedObject::StrokeLineage("crossing".into()),
                SelectedObject::StrokeLineage("inside".into()),
            ]
        );
    }

    #[test]
    fn empty_or_degenerate_lasso_is_idle_and_release_closes_implicitly() {
        let mut session = PageSession::new("page-1");
        session
            .load_committed_strokes(vec![stroke("inside", &[(150.0, 150.0)])])
            .unwrap();
        let bound = BoundPageKey::new("notebook-1", "page-1");
        let page = OpenPage::new(&bound, &session, &[]);
        assert!(select_stroke_lineages(&page, &square()[..2]).is_none());

        let mut state = LassoState::default();
        state.begin_lasso(bound.clone(), square()[0]);
        for point in &square()[1..] {
            assert!(state.push_lasso_point(*point));
        }
        assert!(state.finish_lasso(&page).is_some());
        assert!(matches!(state, LassoState::Selected(_)));
    }

    #[test]
    fn move_is_clamped_and_same_id_replacements_preserve_lineage() {
        let mut session = PageSession::new("page-1");
        session
            .load_committed_strokes_with_roots(
                vec![stroke("fragment", &[(100.0, 100.0), (200.0, 200.0)])],
                [("fragment".into(), "root".into())].into_iter().collect(),
            )
            .unwrap();
        let bound = BoundPageKey::new("notebook-1", "page-1");
        let page = OpenPage::new(&bound, &session, &[]);
        let mut state = LassoState::default();
        state.begin_lasso(bound.clone(), square()[0]);
        for point in &square()[1..] {
            state.push_lasso_point(*point);
        }
        state.finish_lasso(&page).unwrap();
        assert!(state.begin_move(Point::new(150.0, 150.0)));
        let (dx, dy) = state.update_move(Point::new(-500.0, -500.0)).unwrap();
        assert!(dx < 0.0 && dy < 0.0);
        let moved = state.finish_move().unwrap();
        assert_eq!(moved.roots, vec!["root"]);
        let replacements = move_replacements(&page, &moved.roots, moved.dx, moved.dy).unwrap();
        assert_eq!(replacements[0].target_stroke_id, "fragment");
        assert_eq!(replacements[0].fragments[0].id, "fragment");
    }

    #[test]
    fn transient_preview_writes_nothing_and_commit_is_one_atomic_edit() {
        let original = stroke("fragment", &[(100.0, 100.0), (200.0, 200.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&original).unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session
            .load_committed_strokes_with_roots(
                vec![original],
                [("fragment".into(), "root".into())].into_iter().collect(),
            )
            .unwrap();

        let bound = ctrl.open_page().bound().clone();
        let mut state = LassoState::default();
        state.begin_lasso(bound, square()[0]);
        for point in &square()[1..] {
            state.push_lasso_point(*point);
        }
        state.finish_lasso(&ctrl.open_page()).unwrap();
        assert!(state.begin_move(Point::new(150.0, 150.0)));
        state.update_move(Point::new(250.0, 275.0)).unwrap();
        assert_eq!(ctrl.store().replace_append_count, 0, "preview is transient");

        let movement = state.finish_move().unwrap();
        let replacements =
            move_replacements(&ctrl.open_page(), &movement.roots, movement.dx, movement.dy)
                .unwrap();
        ctrl.durable_replace_strokes(&replacements).unwrap();
        assert_eq!(ctrl.store().replace_append_count, 1);
        assert_eq!(ctrl.session.committed_count(), 1);
        assert_eq!(ctrl.session.committed_strokes()[0].id.as_str(), "fragment");
        assert_eq!(ctrl.session.root_id_of(&StrokeId::new("fragment")), "root");
    }

    #[test]
    fn completed_move_dirty_bounds_include_old_and_new_without_becoming_full_page() {
        let old = CanonicalRect {
            x: 100.0,
            y: 200.0,
            width: 300.0,
            height: 400.0,
        };
        let new = CanonicalRect {
            x: 600.0,
            y: 900.0,
            ..old
        };
        let dirty = move_dirty_bounds(old, new);
        assert!(dirty.contains(Point::new(old.x, old.y)));
        assert!(dirty.contains(Point::new(
            old.x + old.width - 1.0,
            old.y + old.height - 1.0
        )));
        assert!(dirty.contains(Point::new(new.x, new.y)));
        assert!(dirty.contains(Point::new(
            new.x + new.width - 1.0,
            new.y + new.height - 1.0
        )));
        assert!(dirty.width < CANONICAL_WIDTH);
        assert!(dirty.height < CANONICAL_HEIGHT);
    }

    #[test]
    fn move_preview_dirty_is_prev_union_current_and_stores_current_only() {
        let a = DirtyRegion::new(100, 100, 50, 50);
        let b = DirtyRegion::new(150, 120, 50, 50);
        let c = DirtyRegion::new(400, 400, 50, 50);
        let d = DirtyRegion::new(800, 900, 50, 50);
        let mut overlay = TouchOverlayDirty {
            move_preview: Some(a),
            ..TouchOverlayDirty::default()
        };
        let paint_ab = overlay.begin_move_preview(b);
        assert_eq!(paint_ab, a.union(b));
        assert_eq!(overlay.move_preview, Some(b));

        let paint_bc = overlay.begin_move_preview(c);
        assert_eq!(paint_bc, b.union(c));
        assert_ne!(
            paint_bc,
            a.union(b).union(c),
            "must not keep A in the paint"
        );

        let paint_cd = overlay.begin_move_preview(d);
        assert_eq!(paint_cd, c.union(d));
        assert_eq!(overlay.move_preview, Some(d));
        assert_ne!(
            overlay.move_preview.unwrap(),
            a.union(b).union(c).union(d),
            "stored footprint after D must be D, not the path"
        );
    }

    #[test]
    fn lasso_outline_dirty_accumulates_every_segment() {
        let s1 = DirtyRegion::new(10, 10, 20, 4);
        let s2 = DirtyRegion::new(25, 10, 20, 4);
        let s3 = DirtyRegion::new(40, 8, 8, 30);
        let mut overlay = TouchOverlayDirty::default();
        overlay.add_lasso_segment(s1);
        overlay.add_lasso_segment(s2);
        overlay.add_lasso_segment(s3);
        let cleanup = overlay.take_cleanup().unwrap();
        assert!(cleanup.intersects(s1));
        assert!(cleanup.intersects(s2));
        assert!(cleanup.intersects(s3));
        assert_eq!(cleanup, s1.union(s2).union(s3));
        assert!(!overlay.is_some());
    }
}
