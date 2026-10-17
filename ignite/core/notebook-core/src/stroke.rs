//! Stroke construction from pen-down / move / pen-up.
//!
//! The builder never begins a stroke implicitly and never discards one silently: every state
//! change the caller must mirror in its operation log is returned from the call that caused it.

#[cfg(feature = "mint")]
use crate::ids::new_stroke_id;
use crate::ids::StrokeId;
use crate::point::Point;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StrokePhase {
    #[default]
    Idle,
    Active,
}

/// Completed or in-progress stroke. Points are canonical page coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    pub id: StrokeId,
    pub points: Vec<Point>,
    pub brush: Option<String>,
    pub finished: bool,
}

impl Stroke {
    pub fn bounds(&self) -> Option<(f32, f32, f32, f32)> {
        let mut iter = self.points.iter();
        let first = iter.next()?;
        let mut min_x = first.x;
        let mut min_y = first.y;
        let mut max_x = first.x;
        let mut max_y = first.y;
        for p in iter {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
        Some((min_x, min_y, max_x, max_y))
    }

    pub fn last_point(&self) -> Option<&Point> {
        self.points.last()
    }
}

/// Result of [`StrokeBuilder::begin`].
///
/// `implicitly_finished` is returned, never dropped, so the caller can emit the matching
/// `FinishStroke` operation before the `BeginStroke` of the new one. A duplicate pen-down can
/// therefore never make a stroke vanish from the operation stream.
#[derive(Clone, Debug, PartialEq)]
#[cfg(feature = "mint")]
pub struct BeginOutcome {
    pub stroke_id: StrokeId,
    pub implicitly_finished: Option<Stroke>,
}

/// Interprets pen-down / move / pen-up into strokes with stable IDs.
#[derive(Debug, Default)]
#[cfg(feature = "mint")]
pub struct StrokeBuilder {
    phase: StrokePhase,
    current: Option<Stroke>,
    finished: Vec<Stroke>,
}

#[cfg(feature = "mint")]
impl StrokeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> StrokePhase {
        self.phase
    }

    pub fn is_active(&self) -> bool {
        self.phase == StrokePhase::Active
    }

    pub fn current(&self) -> Option<&Stroke> {
        self.current.as_ref()
    }

    pub fn current_id(&self) -> Option<StrokeId> {
        self.current.as_ref().map(|s| s.id.clone())
    }

    pub fn finished_strokes(&self) -> &[Stroke] {
        &self.finished
    }

    /// Begin a new stroke. If one was already active it is finished first and returned in
    /// [`BeginOutcome::implicitly_finished`].
    pub fn begin(&mut self, point: Point) -> BeginOutcome {
        let implicitly_finished = self.finish();
        let id = new_stroke_id();
        self.current = Some(Stroke {
            id: id.clone(),
            points: vec![point],
            brush: None,
            finished: false,
        });
        self.phase = StrokePhase::Active;
        BeginOutcome {
            stroke_id: id,
            implicitly_finished,
        }
    }

    /// Append a point while the pen is down. Returns `false` and changes nothing if idle:
    /// the builder never starts a stroke from a move.
    pub fn append(&mut self, point: Point) -> bool {
        if self.phase != StrokePhase::Active {
            return false;
        }
        match self.current.as_mut() {
            Some(stroke) => {
                stroke.points.push(point);
                true
            }
            None => false,
        }
    }

    /// Append `point` unless it is at the same page location as the current last point.
    ///
    /// Returns `true` when a point was actually appended. Used for pen-up, whose coordinate is
    /// frequently a repeat of the final move.
    pub fn append_if_moved(&mut self, point: Point) -> bool {
        if self.phase != StrokePhase::Active {
            return false;
        }
        let duplicate = self
            .current
            .as_ref()
            .and_then(|s| s.last_point())
            .is_some_and(|last| last.same_location(&point));
        if duplicate {
            return false;
        }
        self.append(point)
    }

    /// Finish the active stroke. Returns it if one was active.
    pub fn finish(&mut self) -> Option<Stroke> {
        if self.phase != StrokePhase::Active {
            return None;
        }
        let mut stroke = self.current.take()?;
        stroke.finished = true;
        self.phase = StrokePhase::Idle;
        self.finished.push(stroke.clone());
        Some(stroke)
    }

    /// Cancel the active stroke without treating it as a normal pen-up finish.
    ///
    /// Used when the input stream reports a gap (`SYN_DROPPED`): geometry already drawn may
    /// remain on screen, but the stroke must not bridge the unknown interval. The cancelled
    /// stroke is **not** added to [`StrokeBuilder::finished_strokes`].
    pub fn cancel(&mut self) -> Option<Stroke> {
        if self.phase != StrokePhase::Active {
            return None;
        }
        let mut stroke = self.current.take()?;
        stroke.finished = false;
        self.phase = StrokePhase::Idle;
        Some(stroke)
    }
}

#[cfg(all(test, feature = "mint"))]
mod tests {
    use super::*;

    #[test]
    fn down_move_up_builds_one_stroke() {
        let mut b = StrokeBuilder::new();
        b.begin(Point::new(0.0, 0.0).with_pressure(0.5));
        assert_eq!(b.phase(), StrokePhase::Active);
        assert!(b.append(Point::new(1.0, 1.0).with_pressure(0.6)));
        assert!(b.append(Point::new(2.0, 1.5).with_pressure(0.4)));
        let s = b.finish().expect("stroke");
        assert!(s.finished);
        assert_eq!(s.points.len(), 3);
        assert_eq!(s.points[1].pressure, Some(0.6));
        assert_eq!(b.finished_strokes().len(), 1);
        assert_eq!(b.phase(), StrokePhase::Idle);
    }

    #[test]
    fn move_without_down_is_ignored_and_starts_nothing() {
        let mut b = StrokeBuilder::new();
        assert!(!b.append(Point::new(1.0, 1.0)));
        assert_eq!(b.phase(), StrokePhase::Idle);
        assert!(b.current().is_none());
        assert!(b.finish().is_none());
        assert!(b.finished_strokes().is_empty());
    }

    #[test]
    fn up_without_down_is_noop() {
        let mut b = StrokeBuilder::new();
        assert!(b.finish().is_none());
        assert_eq!(b.phase(), StrokePhase::Idle);
    }

    #[test]
    fn begin_while_active_returns_the_implicitly_finished_stroke() {
        let mut b = StrokeBuilder::new();
        let first = b.begin(Point::new(0.0, 0.0));
        b.append(Point::new(1.0, 0.0));
        let second = b.begin(Point::new(10.0, 10.0));
        let orphan = second
            .implicitly_finished
            .expect("previous stroke must be surfaced, not dropped");
        assert_eq!(orphan.id, first.stroke_id);
        assert!(orphan.finished);
        assert_eq!(orphan.points.len(), 2);
        assert_eq!(b.finished_strokes().len(), 1);
        assert_eq!(b.current().unwrap().points[0].x, 10.0);
        assert_ne!(second.stroke_id, first.stroke_id);
    }

    #[test]
    fn first_begin_finishes_nothing() {
        let mut b = StrokeBuilder::new();
        assert!(b.begin(Point::new(0.0, 0.0)).implicitly_finished.is_none());
    }

    #[test]
    fn append_if_moved_skips_duplicate_location() {
        let mut b = StrokeBuilder::new();
        b.begin(Point::new(0.0, 0.0));
        b.append(Point::new(5.0, 5.0));
        // Same location, different pressure/time: must not duplicate the vertex.
        assert!(!b.append_if_moved(Point::new(5.0, 5.0).with_pressure(0.0).with_time(9)));
        assert_eq!(b.current().unwrap().points.len(), 2);
        assert!(b.append_if_moved(Point::new(6.0, 5.0)));
        assert_eq!(b.current().unwrap().points.len(), 3);
    }

    #[test]
    fn append_if_moved_is_inert_when_idle() {
        let mut b = StrokeBuilder::new();
        assert!(!b.append_if_moved(Point::new(1.0, 1.0)));
        assert!(b.current().is_none());
    }

    #[test]
    fn cancel_abandons_active_stroke_without_finishing() {
        let mut b = StrokeBuilder::new();
        b.begin(Point::new(0.0, 0.0));
        b.append(Point::new(1.0, 1.0));
        let cancelled = b.cancel().expect("active");
        assert!(!cancelled.finished);
        assert_eq!(cancelled.points.len(), 2);
        assert_eq!(b.phase(), StrokePhase::Idle);
        assert!(b.current().is_none());
        assert!(b.finished_strokes().is_empty());
        assert!(b.cancel().is_none());
    }

    #[test]
    fn pressure_propagates() {
        let mut b = StrokeBuilder::new();
        b.begin(Point::new(0.0, 0.0).with_pressure(0.1));
        b.append(Point::new(1.0, 0.0).with_pressure(0.9));
        let s = b.finish().unwrap();
        assert_eq!(s.points[0].pressure, Some(0.1));
        assert_eq!(s.points[1].pressure, Some(0.9));
    }

    #[test]
    fn bounds_of_stroke() {
        let mut b = StrokeBuilder::new();
        b.begin(Point::new(2.0, 5.0));
        b.append(Point::new(8.0, 1.0));
        let s = b.finish().unwrap();
        assert_eq!(s.bounds(), Some((2.0, 1.0, 8.0, 5.0)));
    }
}
