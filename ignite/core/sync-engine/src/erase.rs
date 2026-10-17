//! Deterministic evaluation of `EraseRegion` (the same algorithm as the Kindle's F7 gesture).
//!
//! The Kindle eraser clips its working fragments against a sequence of capsule sweeps:
//! a stationary sweep at the Down point, then one sweep per consecutive pair of path samples
//! (`EraserGestureState::on_down` → `process_to(down)` → `process_to(move…)`). This module runs
//! exactly that sequence through the one shared primitive,
//! [`notebook_core::clip_stroke_against_eraser_sweep`] — there is no second eraser.
//!
//! Scope: only visible strokes whose lineage root is in `observed_roots`. Every other stroke is
//! passed through untouched (the S1 observed-roots rule).
//!
//! Replacement: a source that the sweeps changed is replaced, at its render position, by its
//! surviving pieces in working order. Piece `i` of source `S` gets
//! `StrokeId = UUIDv5(namespace = operation_id, name = "<S>/<i>")`, keeps `S`'s lineage root
//! and style. As on the Kindle, a source whose single surviving piece is geometrically equal
//! (x/y within `f32::EPSILON`) to the original is left untouched.
//!
//! Arithmetic is IEEE f32 add/sub/mul/div plus `round`/`clamp` (pressure uses the F6 12-bit
//! quantized interpolation), so results are bit-identical on every platform.

use std::collections::BTreeSet;

use notebook_core::{clip_stroke_against_eraser_sweep, Point, Stroke, StrokeId as CoreStrokeId};
use sync_protocol::{OperationId, StrokeId, SyncPoint, SyncStroke};
use uuid::Uuid;

/// Result of erasing one page's strokes.
#[derive(Clone, Debug, PartialEq)]
pub struct EraseResult {
    /// New visible strokes in render order.
    pub strokes: Vec<SyncStroke>,
    /// Visible sources that were replaced (their IDs leave the live state).
    pub replaced: Vec<StrokeId>,
}

/// The Kindle gesture's sweep sequence for `path`: `(p0,p0), (p0,p1), (p1,p2), …`.
pub fn sweep_segments(path: &[SyncPoint]) -> Vec<(Point, Point)> {
    let pts: Vec<Point> = path.iter().map(to_core_point).collect();
    let mut out = Vec::with_capacity(pts.len());
    if let Some(&first) = pts.first() {
        out.push((first, first));
    }
    for w in pts.windows(2) {
        out.push((w[0], w[1]));
    }
    out
}

fn to_core_point(p: &SyncPoint) -> Point {
    Point {
        x: p.x,
        y: p.y,
        pressure: p.pressure,
        t_us: None,
    }
}

fn to_core(s: &SyncStroke) -> Stroke {
    Stroke {
        id: CoreStrokeId::new(s.stroke_id.to_string()),
        points: s.points.iter().map(to_core_point).collect(),
        brush: None,
        finished: true,
    }
}

/// Same equality rule as the Kindle gesture (`eraser::points_differ`).
fn points_differ(a: &[Point], b: &[Point]) -> bool {
    a.len() != b.len()
        || a.iter()
            .zip(b)
            .any(|(p, q)| (p.x - q.x).abs() > f32::EPSILON || (p.y - q.y).abs() > f32::EPSILON)
}

/// Deterministic fragment identity (S1 rule).
pub fn fragment_id(operation_id: OperationId, source: StrokeId, index: usize) -> StrokeId {
    let name = format!("{source}/{index}");
    StrokeId::from_uuid(Uuid::new_v5(&operation_id.as_uuid(), name.as_bytes()))
        .expect("UUIDv5 is always a valid StrokeId")
}

/// Evaluate an erase over one page's visible strokes.
pub fn erase_page_strokes(
    strokes: &[SyncStroke],
    observed_roots: &BTreeSet<StrokeId>,
    path: &[SyncPoint],
    radius: f32,
    operation_id: OperationId,
) -> EraseResult {
    let sweeps = sweep_segments(path);
    // (index into `strokes`, working pieces, touched)
    let mut work: Vec<(usize, Vec<Stroke>, bool)> = strokes
        .iter()
        .enumerate()
        .filter(|(_, s)| observed_roots.contains(&s.root_id))
        .map(|(i, s)| (i, vec![to_core(s)], false))
        .collect();

    for (a, b) in &sweeps {
        for (_, pieces, touched) in &mut work {
            let prev = std::mem::take(pieces);
            for piece in prev {
                let clipped = clip_stroke_against_eraser_sweep(&piece, *a, *b, radius);
                if clipped.len() != 1 || points_differ(&clipped[0].points, &piece.points) {
                    *touched = true;
                }
                pieces.extend(clipped);
            }
        }
    }

    let mut out = Vec::with_capacity(strokes.len());
    let mut replaced = Vec::new();
    let mut work_iter = work.into_iter().peekable();
    for (i, s) in strokes.iter().enumerate() {
        let entry = match work_iter.peek() {
            Some((wi, _, _)) if *wi == i => work_iter.next(),
            _ => None,
        };
        let Some((_, pieces, touched)) = entry else {
            out.push(s.clone());
            continue;
        };
        let original = to_core(s);
        let unchanged =
            !touched || (pieces.len() == 1 && !points_differ(&pieces[0].points, &original.points));
        if unchanged {
            out.push(s.clone());
            continue;
        }
        replaced.push(s.stroke_id);
        for (k, piece) in pieces.iter().enumerate() {
            out.push(SyncStroke {
                stroke_id: fragment_id(operation_id, s.stroke_id, k),
                root_id: s.root_id,
                style: s.style,
                points: piece
                    .points
                    .iter()
                    .map(|p| SyncPoint::new(p.x, p.y, p.pressure))
                    .collect(),
            });
        }
    }
    EraseResult {
        strokes: out,
        replaced,
    }
}
