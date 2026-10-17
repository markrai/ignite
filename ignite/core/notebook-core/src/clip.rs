//! Durable partial-stroke eraser clipping (Gate F7).
//!
//! The public entry-point is [`clip_stroke_against_eraser_sweep`], which takes a stroke and
//! a capsule-shaped eraser sweep and returns the surviving fragments of the stroke.

use crate::hit::{point_segment_distance_sq, segment_segment_distance_sq};
use crate::ids::StrokeId;
use crate::point::Point;
use crate::stroke::Stroke;

/// Binary-search iterations for capsule-boundary refinement (~10⁻¹² accuracy on [0,1]).
const BISECT_ITERS: u32 = 40;

/// Ternary-search iterations for the deepest-penetration point.
const TERNARY_ITERS: u32 = 64;

// ─────────────────────────────────────────────────────────────────────────────
// Internal geometry helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Linearly interpolate a synthetic boundary point between `p0` and `p1` at parameter `t`.
///
/// * `x`, `y` are interpolated in the obvious way.
/// * `pressure` uses quantised F6 `interpolate_q` semantics (see [`interp_pressure`]).
/// * `t_us` is not interpolated — always `None` on synthetic points.
fn lerp_point(p0: Point, p1: Point, t: f32) -> Point {
    Point {
        x: p0.x + t * (p1.x - p0.x),
        y: p0.y + t * (p1.y - p0.y),
        pressure: interp_pressure(p0, p1, t),
        t_us: None,
    }
}

/// Quantised pressure interpolation (Gate F6 `interpolate_q` semantics).
///
/// Each endpoint pressure is quantised to a 12-bit integer
/// (`q = round(clamp(p, 0, 1) × 4095)`), the integers are lerped, then converted back.
/// Returns `None` when either pressure is `None` or non-finite.
fn interp_pressure(p0: Point, p1: Point, t: f32) -> Option<f32> {
    let pr0 = p0.pressure.filter(|v| v.is_finite())?;
    let pr1 = p1.pressure.filter(|v| v.is_finite())?;
    let q0 = (pr0.clamp(0.0, 1.0) * 4095.0).round() as i32;
    let q1 = (pr1.clamp(0.0, 1.0) * 4095.0).round() as i32;
    let q = ((1.0 - t) * q0 as f32 + t * q1 as f32).round() as i32;
    Some(q.clamp(0, 4095) as f32 / 4095.0)
}

/// True iff `p` lies inside (or on the boundary of) the capsule.
#[inline]
fn pt_in_capsule(p: Point, e0: Point, e1: Point, r2: f32) -> bool {
    point_segment_distance_sq(p, e0, e1) <= r2
}

/// Find the `t ∈ [0,1]` that minimises `dist(lerp(p0,p1,t), sweep_segment)`.
///
/// Uses ternary search — valid because the distance from a point to a convex set is
/// a convex function, so its composition with the linear map `t ↦ lerp(p0,p1,t)` is
/// also convex.
fn find_min_dist_t(p0: Point, p1: Point, e0: Point, e1: Point) -> f32 {
    let mut lo = 0.0_f32;
    let mut hi = 1.0_f32;
    for _ in 0..TERNARY_ITERS {
        let m1 = lo + (hi - lo) / 3.0;
        let m2 = hi - (hi - lo) / 3.0;
        let d1 = point_segment_distance_sq(lerp_point(p0, p1, m1), e0, e1);
        let d2 = point_segment_distance_sq(lerp_point(p0, p1, m2), e0, e1);
        if d1 < d2 {
            hi = m2;
        } else {
            lo = m1;
        }
    }
    (lo + hi) * 0.5
}

/// Binary-search for the capsule entry crossing.
///
/// Preconditions: `lerp(p0,p1,lo)` is **outside**, `lerp(p0,p1,hi)` is **inside**.
fn bisect_entry(
    p0: Point,
    p1: Point,
    e0: Point,
    e1: Point,
    r2: f32,
    mut lo: f32,
    mut hi: f32,
) -> f32 {
    for _ in 0..BISECT_ITERS {
        let mid = (lo + hi) * 0.5;
        if pt_in_capsule(lerp_point(p0, p1, mid), e0, e1, r2) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    (lo + hi) * 0.5
}

/// Binary-search for the capsule exit crossing.
///
/// Preconditions: `lerp(p0,p1,lo)` is **inside**, `lerp(p0,p1,hi)` is **outside**.
fn bisect_exit(
    p0: Point,
    p1: Point,
    e0: Point,
    e1: Point,
    r2: f32,
    mut lo: f32,
    mut hi: f32,
) -> f32 {
    for _ in 0..BISECT_ITERS {
        let mid = (lo + hi) * 0.5;
        if pt_in_capsule(lerp_point(p0, p1, mid), e0, e1, r2) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) * 0.5
}

/// Flush accumulated fragment points as a new [`Stroke`] onto `out`, clearing `frag`.
fn push_fragment(frag: &mut Vec<Point>, out: &mut Vec<Stroke>, src: &Stroke) {
    if frag.is_empty() {
        return;
    }
    let pts = std::mem::take(frag);
    out.push(Stroke {
        id: StrokeId::new("pending"),
        points: pts,
        brush: src.brush.clone(),
        finished: true,
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Clip a stroke against a capsule-shaped eraser sweep and return the surviving fragments.
///
/// ## Geometry
/// The eraser sweep is defined by `sweep_start`→`sweep_end` with `radius`, all in
/// canonical page coordinates. A point `p` lies inside the capsule iff
/// `dist(p, sweep_segment) ≤ radius`.
///
/// Degenerate sweeps (`sweep_start ≈ sweep_end`) are treated as a disk of `radius` at
/// `sweep_start` — handled automatically by [`point_segment_distance_sq`]'s degenerate-
/// segment fallback.
///
/// ## Algorithm
/// For each polyline segment the endpoints are classified inside/outside the capsule.
/// Transition crossings are binary-searched to ~10⁻¹² accuracy (40 iterations).
/// When both endpoints are outside but the segment dips through the capsule, the deepest
/// point is located via ternary search (64 iterations), providing anchors for both the
/// entry and exit binary searches.
///
/// ## Returned fragments
/// Each [`Stroke`] in the returned `Vec` has:
/// - `brush` cloned from `stroke`
/// - `finished = true`
/// - `id = StrokeId::new("pending")` — the **caller must assign real IDs**
///
/// Empty fragments are never returned. Single-point fragments are valid.
/// Fragment point order matches the source direction.
pub fn clip_stroke_against_eraser_sweep(
    stroke: &Stroke,
    sweep_start: Point,
    sweep_end: Point,
    radius: f32,
) -> Vec<Stroke> {
    let pts = &stroke.points;
    if pts.is_empty() {
        return vec![];
    }

    let r2 = radius * radius;
    let e0 = sweep_start;
    let e1 = sweep_end;

    // Detect degenerate sweep (eraser = disk).
    // `segment_segment_distance_sq` has a known numerical degeneracy when its second
    // segment collapses to a point; use `point_segment_distance_sq` for that case.
    let sweep_degenerate = {
        let dx = e1.x - e0.x;
        let dy = e1.y - e0.y;
        dx * dx + dy * dy <= f32::EPSILON
    };

    let inside = |p: Point| pt_in_capsule(p, e0, e1, r2);

    // True iff a stroke segment with both endpoints OUTSIDE still dips into the capsule.
    let segment_dips = |p0: Point, p1: Point| -> bool {
        if sweep_degenerate {
            // Min distance from the disk centre to the stroke segment.
            point_segment_distance_sq(e0, p0, p1) <= r2
        } else {
            segment_segment_distance_sq(p0, p1, e0, e1) <= r2
        }
    };

    let mut out: Vec<Stroke> = Vec::new();
    let mut frag: Vec<Point> = Vec::new();
    let mut erased = inside(pts[0]);

    if !erased {
        frag.push(pts[0]);
    }

    for i in 0..pts.len().saturating_sub(1) {
        let p0 = pts[i];
        let p1 = pts[i + 1];
        let i1 = inside(p1);

        match (erased, i1) {
            // ── Outside → Outside ─────────────────────────────────────────────
            (false, false) => {
                if segment_dips(p0, p1) {
                    // The segment passes through the capsule even though both endpoints
                    // are outside. Find the deepest penetration point via ternary search,
                    // then binary-search for entry and exit boundary parameters.
                    let t_deep = find_min_dist_t(p0, p1, e0, e1);
                    let pt_deep = lerp_point(p0, p1, t_deep);
                    if inside(pt_deep) {
                        let t_in = bisect_entry(p0, p1, e0, e1, r2, 0.0, t_deep);
                        let t_out = bisect_exit(p0, p1, e0, e1, r2, t_deep, 1.0);
                        frag.push(lerp_point(p0, p1, t_in));
                        push_fragment(&mut frag, &mut out, stroke);
                        frag = vec![lerp_point(p0, p1, t_out), p1];
                        // `erased` remains false — we exit the capsule within this segment.
                    } else {
                        // Borderline float case: ternary search landed outside.
                        // Treat as no-dip.
                        frag.push(p1);
                    }
                } else {
                    frag.push(p1);
                }
            }

            // ── Outside → Inside ──────────────────────────────────────────────
            (false, true) => {
                let t_in = bisect_entry(p0, p1, e0, e1, r2, 0.0, 1.0);
                frag.push(lerp_point(p0, p1, t_in));
                push_fragment(&mut frag, &mut out, stroke);
                erased = true;
            }

            // ── Inside → Outside ──────────────────────────────────────────────
            (true, false) => {
                let t_out = bisect_exit(p0, p1, e0, e1, r2, 0.0, 1.0);
                frag = vec![lerp_point(p0, p1, t_out), p1];
                erased = false;
            }

            // ── Inside → Inside ───────────────────────────────────────────────
            (true, true) => {}
        }
    }

    // Flush any trailing outside fragment.
    if !erased {
        push_fragment(&mut frag, &mut out, stroke);
    }

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hit::ERASER_HIT_RADIUS_CANONICAL;

    const R: f32 = ERASER_HIT_RADIUS_CANONICAL; // 240.0

    /// Allowed position error on computed boundary points (binary search ≈ 10⁻¹²).
    const TOL: f32 = 0.02;

    fn mk_stroke(points: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new("test"),
            points: points.iter().map(|&(x, y)| Point::new(x, y)).collect(),
            brush: None,
            finished: true,
        }
    }

    fn mk_stroke_pressured(points: &[(f32, f32, Option<f32>)]) -> Stroke {
        Stroke {
            id: StrokeId::new("test"),
            points: points
                .iter()
                .map(|&(x, y, p)| {
                    let pt = Point::new(x, y);
                    match p {
                        Some(pressure) => pt.with_pressure(pressure),
                        None => pt,
                    }
                })
                .collect(),
            brush: None,
            finished: true,
        }
    }

    // ── 1. Horizontal stroke, eraser sweeps through the middle ────────────────
    #[test]
    fn erase_horizontal_middle() {
        // Stroke (0,0)→(2000,0); vertical eraser sweep at x=1000, radius=240.
        // Entry at x=760, exit at x=1240.
        let stroke = mk_stroke(&[(0.0, 0.0), (2000.0, 0.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(1000.0, -2000.0),
            Point::new(1000.0, 2000.0),
            R,
        );
        assert_eq!(frags.len(), 2, "expected 2 fragments");

        // Fragment 0: (0,0) → (~760,0)
        assert!((frags[0].points.first().unwrap().x).abs() < TOL);
        let x_in = frags[0].points.last().unwrap().x;
        assert!((x_in - (1000.0 - R)).abs() < TOL, "entry x={x_in}");

        // Fragment 1: (~1240,0) → (2000,0)
        let x_out = frags[1].points.first().unwrap().x;
        assert!((x_out - (1000.0 + R)).abs() < TOL, "exit x={x_out}");
        assert!((frags[1].points.last().unwrap().x - 2000.0).abs() < TOL);

        // Brush and finished propagated
        assert_eq!(frags[0].brush, stroke.brush);
        assert!(frags[0].finished);
    }

    // ── 2. Vertical stroke, eraser sweeps through the middle ──────────────────
    #[test]
    fn erase_vertical_middle() {
        // Stroke (500,0)→(500,2000); horizontal eraser sweep at y=1000.
        let stroke = mk_stroke(&[(500.0, 0.0), (500.0, 2000.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(-2000.0, 1000.0),
            Point::new(2000.0, 1000.0),
            R,
        );
        assert_eq!(frags.len(), 2, "expected 2 fragments");

        let y_in = frags[0].points.last().unwrap().y;
        assert!((y_in - (1000.0 - R)).abs() < TOL, "entry y={y_in}");

        let y_out = frags[1].points.first().unwrap().y;
        assert!((y_out - (1000.0 + R)).abs() < TOL, "exit y={y_out}");
    }

    // ── 3. Erase the start of the stroke ──────────────────────────────────────
    #[test]
    fn erase_start() {
        // Eraser centred at x=0 (start), stroke points rightward.
        let stroke = mk_stroke(&[(0.0, 0.0), (2000.0, 0.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(0.0, -2000.0),
            Point::new(0.0, 2000.0),
            R,
        );
        assert_eq!(frags.len(), 1, "expected 1 shortened fragment");

        // Fragment begins near the eraser boundary (x≈240) and ends at (2000,0).
        let x0 = frags[0].points.first().unwrap().x;
        assert!((x0 - R).abs() < TOL, "start x={x0}");
        assert!((frags[0].points.last().unwrap().x - 2000.0).abs() < TOL);
    }

    // ── 4. Erase the end of the stroke ────────────────────────────────────────
    #[test]
    fn erase_end() {
        // Eraser centred at x=2000 (end).
        let stroke = mk_stroke(&[(0.0, 0.0), (2000.0, 0.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(2000.0, -2000.0),
            Point::new(2000.0, 2000.0),
            R,
        );
        assert_eq!(frags.len(), 1, "expected 1 shortened fragment");

        assert!((frags[0].points.first().unwrap().x).abs() < TOL);
        let x_last = frags[0].points.last().unwrap().x;
        assert!((x_last - (2000.0 - R)).abs() < TOL, "end x={x_last}");
    }

    // ── 5. Erase entire stroke ────────────────────────────────────────────────
    #[test]
    fn erase_entire() {
        // Short stroke fully inside the eraser disk.
        let stroke = mk_stroke(&[(490.0, 0.0), (510.0, 0.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(500.0, 0.0), // degenerate sweep = disk
            Point::new(500.0, 0.0),
            R,
        );
        assert_eq!(frags.len(), 0, "stroke fully erased");
    }

    // ── 6. Eraser misses completely → 1 unchanged fragment ────────────────────
    #[test]
    fn miss() {
        let stroke = mk_stroke(&[(0.0, 0.0), (1000.0, 0.0), (2000.0, 0.0)]);
        // Eraser sweeps parallel to the stroke but more than `R` above it.
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(0.0, R + 100.0),
            Point::new(2000.0, R + 100.0),
            R,
        );
        assert_eq!(frags.len(), 1, "expected 1 uncut fragment");
        assert_eq!(frags[0].points.len(), 3, "same point count as source");
        assert_eq!(
            frags[0].points, stroke.points,
            "points must be bit-identical"
        );
    }

    // ── 7a. Single-point stroke inside eraser → 0 fragments ──────────────────
    #[test]
    fn single_point_hit() {
        let stroke = mk_stroke(&[(500.0, 0.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(500.0, 0.0),
            Point::new(500.0, 0.0),
            R,
        );
        assert_eq!(frags.len(), 0, "single hit point must be erased");
    }

    // ── 7b. Single-point stroke outside eraser → 1 fragment ──────────────────
    #[test]
    fn single_point_miss() {
        let stroke = mk_stroke(&[(500.0, 0.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(500.0, R + 100.0),
            Point::new(500.0, R + 100.0),
            R,
        );
        assert_eq!(frags.len(), 1, "single miss point must survive");
        assert_eq!(frags[0].points.len(), 1);
        assert_eq!(frags[0].points[0], stroke.points[0]);
    }

    // ── 8. Deterministic quantised pressure interpolation ─────────────────────
    //
    // Setup: stroke (0,0)→(1000,0) with P0.pressure=0.2 and P1.pressure=0.8.
    // Degenerate eraser disk at (500,0) with radius=100 clips at exactly t=0.4 and t=0.6.
    //
    // Expected pressures:
    //   q0 = round(0.2 × 4095) = 819
    //   q1 = round(0.8 × 4095) = 3276
    //   entry (t=0.4): q = round(0.6×819 + 0.4×3276) = round(1801.8) = 1802  → 1802/4095
    //   exit  (t=0.6): q = round(0.4×819 + 0.6×3276) = round(2293.2) = 2293  → 2293/4095
    #[test]
    fn pressure_boundary_quantised() {
        let stroke = mk_stroke_pressured(&[(0.0, 0.0, Some(0.2)), (1000.0, 0.0, Some(0.8))]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(500.0, 0.0),
            Point::new(500.0, 0.0), // degenerate disk
            100.0,
        );
        assert_eq!(frags.len(), 2, "expected 2 fragments");

        let p_entry = frags[0].points.last().unwrap().pressure;
        let p_exit = frags[1].points.first().unwrap().pressure;

        let expected_entry = 1802_f32 / 4095.0_f32;
        let expected_exit = 2293_f32 / 4095.0_f32;

        assert_eq!(p_entry, Some(expected_entry), "entry pressure mismatch");
        assert_eq!(p_exit, Some(expected_exit), "exit pressure mismatch");
    }

    // ── 9. P0.pressure = None, P1.pressure = Some → boundary pressure = None ─
    #[test]
    fn pressure_none_propagates() {
        let stroke = mk_stroke_pressured(&[
            (0.0, 0.0, None), // no pressure data
            (1000.0, 0.0, Some(0.8)),
        ]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(500.0, 0.0),
            Point::new(500.0, 0.0),
            100.0,
        );
        assert_eq!(frags.len(), 2, "expected 2 fragments");

        assert_eq!(
            frags[0].points.last().unwrap().pressure,
            None,
            "entry boundary pressure must be None when P0.pressure is None"
        );
        assert_eq!(
            frags[1].points.first().unwrap().pressure,
            None,
            "exit boundary pressure must be None when P0.pressure is None"
        );
    }

    #[test]
    fn pressure_some_none_boundary_none() {
        let stroke = mk_stroke_pressured(&[(0.0, 0.0, Some(0.2)), (1000.0, 0.0, None)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(500.0, 0.0),
            Point::new(500.0, 0.0),
            100.0,
        );
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0].points.last().unwrap().pressure, None);
        assert_eq!(frags[1].points.first().unwrap().pressure, None);
    }

    #[test]
    fn diagonal_middle_two_fragments() {
        let stroke = mk_stroke(&[(0.0, 0.0), (2000.0, 2000.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(700.0, 1300.0),
            Point::new(1300.0, 700.0),
            R,
        );
        assert_eq!(frags.len(), 2);
    }

    #[test]
    fn sweep_jump_creates_gap() {
        let stroke = mk_stroke(&[(0.0, 1000.0), (2000.0, 1000.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(1000.0, 0.0),
            Point::new(1000.0, 2000.0),
            R,
        );
        assert_eq!(frags.len(), 2);
    }

    #[test]
    fn polyline_two_crossings() {
        let stroke = mk_stroke(&[(0.0, 0.0), (0.0, 2000.0), (2000.0, 2000.0), (2000.0, 0.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(-500.0, 1000.0),
            Point::new(2500.0, 1000.0),
            R,
        );
        assert!(frags.len() >= 2, "got {}", frags.len());
    }

    #[test]
    fn sequential_sweeps_accumulate_three() {
        let stroke = mk_stroke(&[(0.0, 0.0), (3000.0, 0.0)]);
        let fr1 = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(600.0, -10.0),
            Point::new(800.0, 10.0),
            R,
        );
        assert_eq!(fr1.len(), 2);
        let fr2 = clip_stroke_against_eraser_sweep(
            &fr1[1],
            Point::new(2000.0, -10.0),
            Point::new(2200.0, 10.0),
            R,
        );
        assert_eq!(1 + fr2.len(), 3);
    }

    #[test]
    fn back_and_forth_no_resurrection() {
        let stroke = mk_stroke(&[(0.0, 0.0), (2000.0, 0.0)]);
        let fr1 = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(900.0, -10.0),
            Point::new(1100.0, 10.0),
            R,
        );
        assert_eq!(fr1.len(), 2);
        let mut all = Vec::new();
        for piece in &fr1 {
            all.extend(clip_stroke_against_eraser_sweep(
                piece,
                Point::new(1100.0, 10.0),
                Point::new(900.0, -10.0),
                R,
            ));
        }
        assert_eq!(all.len(), 2);
        assert!(all[0].points.last().unwrap().x < all[1].points.first().unwrap().x);
    }

    #[test]
    fn style_inherited_pressure() {
        let mut stroke = mk_stroke(&[(0.0, 0.0), (2000.0, 0.0)]);
        stroke.brush = Some("pressure-width-v1".into());
        let frags = clip_stroke_against_eraser_sweep(
            &stroke,
            Point::new(900.0, 0.0),
            Point::new(1100.0, 0.0),
            R,
        );
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0].brush.as_deref(), Some("pressure-width-v1"));
        assert_eq!(frags[1].brush.as_deref(), Some("pressure-width-v1"));
    }
}
