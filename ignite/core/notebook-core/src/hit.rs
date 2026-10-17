//! Canonical-space stroke hit testing for whole-stroke eraser (Gates F3 / F5).

use crate::point::Point;
use crate::stroke::Stroke;
use crate::StrokeId;

/// Eraser hit radius in canonical page units (~28.6 panel px at 8.4:1).
pub const ERASER_HIT_RADIUS_CANONICAL: f32 = 240.0;

/// Squared distance from point `p` to line segment `a`→`b`.
pub fn point_segment_distance_sq(p: Point, a: Point, b: Point) -> f32 {
    let abx = b.x - a.x;
    let aby = b.y - a.y;
    let apx = p.x - a.x;
    let apy = p.y - a.y;
    let ab_len_sq = abx * abx + aby * aby;
    if ab_len_sq <= f32::EPSILON {
        // Degenerate segment → point distance.
        return apx * apx + apy * apy;
    }
    let t = ((apx * abx + apy * aby) / ab_len_sq).clamp(0.0, 1.0);
    let cx = a.x + t * abx;
    let cy = a.y + t * aby;
    let dx = p.x - cx;
    let dy = p.y - cy;
    dx * dx + dy * dy
}

/// Squared distance between closest points of segments `a0`→`a1` and `b0`→`b1`.
#[allow(unused_assignments)]
pub fn segment_segment_distance_sq(a0: Point, a1: Point, b0: Point, b1: Point) -> f32 {
    let ux = a1.x - a0.x;
    let uy = a1.y - a0.y;
    let vx = b1.x - b0.x;
    let vy = b1.y - b0.y;
    let wx = a0.x - b0.x;
    let wy = a0.y - b0.y;
    let a = ux * ux + uy * uy;
    let b = ux * vx + uy * vy;
    let c = vx * vx + vy * vy;
    let d = ux * wx + uy * wy;
    let e = vx * wx + vy * wy;
    let denom = a * c - b * b;

    let (mut s_n, mut s_d) = (0.0_f32, denom);
    let (mut t_n, mut t_d) = (0.0_f32, denom);

    if denom <= f32::EPSILON {
        s_n = 0.0;
        s_d = 1.0;
        t_n = e;
        t_d = c;
    } else {
        s_n = b * e - c * d;
        t_n = a * e - b * d;
        if s_n < 0.0 {
            s_n = 0.0;
            t_n = e;
            t_d = c;
        } else if s_n > s_d {
            s_n = s_d;
            t_n = e + b;
            t_d = c;
        }
    }

    if t_n < 0.0 {
        t_n = 0.0;
        if -d < 0.0 {
            s_n = 0.0;
        } else if -d > a {
            s_n = s_d;
        } else {
            s_n = -d;
            s_d = a;
        }
    } else if t_n > t_d {
        t_n = t_d;
        if (-d + b) < 0.0 {
            s_n = 0.0;
        } else if (-d + b) > a {
            s_n = s_d;
        } else {
            s_n = -d + b;
            s_d = a;
        }
    }

    let sc = if s_d.abs() <= f32::EPSILON {
        0.0
    } else {
        s_n / s_d
    };
    let tc = if t_d.abs() <= f32::EPSILON {
        0.0
    } else {
        t_n / t_d
    };

    let dx = wx + sc * ux - tc * vx;
    let dy = wy + sc * uy - tc * vy;
    dx * dx + dy * dy
}

/// True if `p` is within `radius` of any segment (or single point) of `stroke`.
pub fn stroke_hit_test(stroke: &Stroke, p: Point, radius: f32) -> bool {
    let r2 = radius * radius;
    let pts = &stroke.points;
    if pts.is_empty() {
        return false;
    }
    if pts.len() == 1 {
        let dx = p.x - pts[0].x;
        let dy = p.y - pts[0].y;
        return dx * dx + dy * dy <= r2;
    }
    for w in pts.windows(2) {
        if point_segment_distance_sq(p, w[0], w[1]) <= r2 {
            return true;
        }
    }
    false
}

/// True if any stroke geometry lies within `radius` of the eraser sweep segment.
///
/// Degenerate sweep (`start` ≈ `end`) falls back to a point hit test at `sweep_start`.
pub fn stroke_intersects_eraser_sweep(
    stroke: &Stroke,
    sweep_start: Point,
    sweep_end: Point,
    radius: f32,
) -> bool {
    let r2 = radius * radius;
    let pts = &stroke.points;
    if pts.is_empty() {
        return false;
    }
    let sdx = sweep_end.x - sweep_start.x;
    let sdy = sweep_end.y - sweep_start.y;
    if sdx * sdx + sdy * sdy <= f32::EPSILON {
        return stroke_hit_test(stroke, sweep_start, radius);
    }
    if pts.len() == 1 {
        return point_segment_distance_sq(pts[0], sweep_start, sweep_end) <= r2;
    }
    for w in pts.windows(2) {
        if segment_segment_distance_sq(w[0], w[1], sweep_start, sweep_end) <= r2 {
            return true;
        }
    }
    false
}

/// Search visible strokes in reverse render order (newest first). Returns first hit.
pub fn hit_test_topmost_stroke<'a>(
    strokes: impl IntoIterator<Item = &'a Stroke>,
    p: Point,
    radius: f32,
) -> Option<&'a StrokeId> {
    let collected: Vec<&Stroke> = strokes.into_iter().collect();
    for stroke in collected.iter().rev() {
        if stroke_hit_test(stroke, p, radius) {
            return Some(&stroke.id);
        }
    }
    None
}

/// All visible strokes intersecting the eraser sweep, newest/topmost first.
pub fn hit_test_strokes_crossing_sweep<'a>(
    strokes: impl IntoIterator<Item = &'a Stroke>,
    sweep_start: Point,
    sweep_end: Point,
    radius: f32,
) -> Vec<&'a StrokeId> {
    let collected: Vec<&Stroke> = strokes.into_iter().collect();
    let mut out = Vec::new();
    for stroke in collected.iter().rev() {
        if stroke_intersects_eraser_sweep(stroke, sweep_start, sweep_end, radius) {
            out.push(&stroke.id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::StrokeId;

    fn stroke(id: &str, points: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: points.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            brush: None,
            finished: true,
        }
    }

    #[test]
    fn near_segment_hits() {
        let s = stroke("a", &[(0.0, 0.0), (1000.0, 0.0)]);
        assert!(stroke_hit_test(
            &s,
            Point::new(500.0, ERASER_HIT_RADIUS_CANONICAL - 1.0),
            ERASER_HIT_RADIUS_CANONICAL
        ));
    }

    #[test]
    fn outside_radius_misses() {
        let s = stroke("a", &[(0.0, 0.0), (1000.0, 0.0)]);
        assert!(!stroke_hit_test(
            &s,
            Point::new(500.0, ERASER_HIT_RADIUS_CANONICAL + 1.0),
            ERASER_HIT_RADIUS_CANONICAL
        ));
    }

    #[test]
    fn single_point_stroke_hittable() {
        let s = stroke("p", &[(100.0, 200.0)]);
        assert!(stroke_hit_test(
            &s,
            Point::new(100.0 + ERASER_HIT_RADIUS_CANONICAL - 1.0, 200.0),
            ERASER_HIT_RADIUS_CANONICAL
        ));
        assert!(!stroke_hit_test(
            &s,
            Point::new(100.0 + ERASER_HIT_RADIUS_CANONICAL + 1.0, 200.0),
            ERASER_HIT_RADIUS_CANONICAL
        ));
    }

    #[test]
    fn overlapping_newest_wins() {
        let older = stroke("old", &[(0.0, 0.0), (1000.0, 0.0)]);
        let newer = stroke("new", &[(500.0, -100.0), (500.0, 100.0)]);
        let id = hit_test_topmost_stroke(
            [&older, &newer],
            Point::new(500.0, 0.0),
            ERASER_HIT_RADIUS_CANONICAL,
        );
        assert_eq!(id.map(|x| x.as_str()), Some("new"));
    }

    #[test]
    fn radius_boundary_deterministic() {
        let s = stroke("a", &[(0.0, 0.0), (1000.0, 0.0)]);
        // Exactly on boundary counts as hit (<=).
        assert!(stroke_hit_test(
            &s,
            Point::new(500.0, ERASER_HIT_RADIUS_CANONICAL),
            ERASER_HIT_RADIUS_CANONICAL
        ));
    }

    #[test]
    fn deleted_stroke_not_considered() {
        let remaining = stroke("keep", &[(0.0, 1000.0), (100.0, 1000.0)]);
        let id = hit_test_topmost_stroke(
            [&remaining],
            Point::new(50.0, 0.0),
            ERASER_HIT_RADIUS_CANONICAL,
        );
        assert!(id.is_none());
    }

    #[test]
    fn sweep_crossing_between_samples_hits() {
        // Vertical stroke at x=500; eraser jumps from left to right without sampling on the stroke.
        let s = stroke("v", &[(500.0, 0.0), (500.0, 1000.0)]);
        assert!(stroke_intersects_eraser_sweep(
            &s,
            Point::new(0.0, 500.0),
            Point::new(1000.0, 500.0),
            ERASER_HIT_RADIUS_CANONICAL,
        ));
    }

    #[test]
    fn parallel_sweep_outside_radius_misses() {
        let s = stroke("h", &[(0.0, 0.0), (1000.0, 0.0)]);
        assert!(!stroke_intersects_eraser_sweep(
            &s,
            Point::new(0.0, ERASER_HIT_RADIUS_CANONICAL + 10.0),
            Point::new(1000.0, ERASER_HIT_RADIUS_CANONICAL + 10.0),
            ERASER_HIT_RADIUS_CANONICAL,
        ));
    }

    #[test]
    fn single_point_stroke_sweep_hit() {
        let s = stroke("p", &[(500.0, 500.0)]);
        assert!(stroke_intersects_eraser_sweep(
            &s,
            Point::new(0.0, 500.0),
            Point::new(1000.0, 500.0),
            ERASER_HIT_RADIUS_CANONICAL,
        ));
    }

    #[test]
    fn fast_long_sweep_hits_several_newest_first() {
        let a = stroke("a", &[(100.0, 0.0), (100.0, 1000.0)]);
        let b = stroke("b", &[(400.0, 0.0), (400.0, 1000.0)]);
        let c = stroke("c", &[(700.0, 0.0), (700.0, 1000.0)]);
        let ids = hit_test_strokes_crossing_sweep(
            [&a, &b, &c],
            Point::new(0.0, 500.0),
            Point::new(800.0, 500.0),
            ERASER_HIT_RADIUS_CANONICAL,
        );
        assert_eq!(
            ids.iter().map(|id| id.as_str()).collect::<Vec<_>>(),
            vec!["c", "b", "a"]
        );
    }

    #[test]
    fn overlapping_sweep_newest_first_order() {
        let older = stroke("old", &[(0.0, 0.0), (1000.0, 0.0)]);
        let newer = stroke("new", &[(0.0, 0.0), (1000.0, 0.0)]);
        let ids = hit_test_strokes_crossing_sweep(
            [&older, &newer],
            Point::new(500.0, -50.0),
            Point::new(500.0, 50.0),
            ERASER_HIT_RADIUS_CANONICAL,
        );
        assert_eq!(
            ids.iter().map(|id| id.as_str()).collect::<Vec<_>>(),
            vec!["new", "old"]
        );
    }

    #[test]
    fn stationary_down_on_stroke_hits() {
        let s = stroke("a", &[(0.0, 0.0), (1000.0, 0.0)]);
        assert!(stroke_intersects_eraser_sweep(
            &s,
            Point::new(500.0, 0.0),
            Point::new(500.0, 0.0),
            ERASER_HIT_RADIUS_CANONICAL,
        ));
    }
}
