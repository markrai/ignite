//! Host-testable pen session: wet ink, committed strokes, and lineage (no framebuffer, no texts).

use std::collections::HashMap;

use notebook_core::{NotebookOp, PagePoint, Point, Stroke, StrokeBuilder, StrokeId, StrokePhase};

/// Application-level stroke phase for Gate F0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SessionPhase {
    #[default]
    Idle,
    WetStrokeActive,
    FinishingStroke,
}

/// Pen session for one bound page. Not the page document: texts, selection, and renderer
/// caches live on `StrokeController` / `OpenPage` / `TextOverlay`.
#[derive(Debug)]
pub struct PageSession {
    page_id: String,
    phase: SessionPhase,
    builder: StrokeBuilder,
    committed: Vec<Stroke>,
    /// Visible stroke id → authored root lineage id.
    root_of: HashMap<String, String>,
    /// Session-local [`NotebookOp`] stream for StrokeBuilder consistency. Not persisted and
    /// not `SyncOperationV1`.
    #[allow(dead_code)]
    ops: Vec<NotebookOp>,
    strokes_started: u64,
    strokes_committed: u64,
    strokes_cancelled: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DownOutcome {
    pub stroke_id: StrokeId,
    pub point: Point,
    pub ops: Vec<NotebookOp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MoveOutcome {
    pub stroke_id: StrokeId,
    pub point: Point,
    pub appended: bool,
    pub ops: Vec<NotebookOp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UpOutcome {
    pub stroke: Stroke,
    pub ops: Vec<NotebookOp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GapOutcome {
    pub cancelled: Option<Stroke>,
    pub ops: Vec<NotebookOp>,
}

impl PageSession {
    pub fn new(page_id: impl Into<String>) -> Self {
        Self {
            page_id: page_id.into(),
            phase: SessionPhase::Idle,
            builder: StrokeBuilder::new(),
            committed: Vec::new(),
            root_of: HashMap::new(),
            ops: Vec::new(),
            strokes_started: 0,
            strokes_committed: 0,
            strokes_cancelled: 0,
        }
    }

    pub fn phase(&self) -> SessionPhase {
        self.phase
    }

    pub fn committed_strokes(&self) -> &[Stroke] {
        &self.committed
    }

    pub fn committed_count(&self) -> usize {
        self.committed.len()
    }

    /// Newest currently visible committed stroke id (visible order after tombstones).
    pub fn last_committed_stroke_id(&self) -> Option<&StrokeId> {
        self.committed.last().map(|s| &s.id)
    }

    pub fn strokes_started(&self) -> u64 {
        self.strokes_started
    }

    pub fn strokes_committed(&self) -> u64 {
        self.strokes_committed
    }

    pub fn strokes_cancelled(&self) -> u64 {
        self.strokes_cancelled
    }

    pub fn is_idle(&self) -> bool {
        self.phase == SessionPhase::Idle
    }

    pub fn is_wet_active(&self) -> bool {
        self.phase == SessionPhase::WetStrokeActive
    }

    pub fn page_id(&self) -> &str {
        &self.page_id
    }

    /// Replace the bound page while Idle. Clears committed model; loads `strokes`.
    /// Does not emit live ops or bump started/committed counters.
    #[allow(dead_code)] // host tests; runtime prefers [`Self::rebind_page_with_roots`]
    pub fn rebind_page(
        &mut self,
        page_id: impl Into<String>,
        strokes: Vec<Stroke>,
    ) -> Result<(), String> {
        self.rebind_page_with_roots(page_id, strokes, HashMap::new())
    }

    /// Rebind with explicit lineage map (empty map → each stroke roots to itself).
    pub fn rebind_page_with_roots(
        &mut self,
        page_id: impl Into<String>,
        strokes: Vec<Stroke>,
        root_of: HashMap<String, String>,
    ) -> Result<(), String> {
        if self.phase != SessionPhase::Idle {
            return Err("rebind_page requires Idle".into());
        }
        if self.builder.is_active() {
            return Err("rebind_page: builder not idle".into());
        }
        self.page_id = page_id.into();
        self.committed.clear();
        self.root_of.clear();
        self.ops.clear();
        let roots = if root_of.is_empty() {
            strokes
                .iter()
                .map(|s| {
                    let id = s.id.as_str().to_string();
                    (id.clone(), id)
                })
                .collect()
        } else {
            root_of
        };
        self.load_committed_strokes_with_roots(strokes, roots)
    }

    /// Load already-finished canonical strokes (restart reconstruction). No live ops emitted.
    /// Each stroke's root lineage defaults to its own id.
    #[allow(dead_code)] // host tests; runtime prefers [`Self::load_committed_strokes_with_roots`]
    pub fn load_committed_strokes(&mut self, strokes: Vec<Stroke>) -> Result<(), String> {
        let roots: HashMap<String, String> = strokes
            .iter()
            .map(|s| {
                let id = s.id.as_str().to_string();
                (id.clone(), id)
            })
            .collect();
        self.load_committed_strokes_with_roots(strokes, roots)
    }

    /// Load visible strokes with explicit authored-stroke lineage map.
    /// Appends to the current committed set (caller clears first on rebind).
    pub fn load_committed_strokes_with_roots(
        &mut self,
        strokes: Vec<Stroke>,
        root_of: HashMap<String, String>,
    ) -> Result<(), String> {
        if self.phase != SessionPhase::Idle {
            return Err("load_committed_strokes requires Idle".into());
        }
        if self.builder.is_active() {
            return Err("load_committed_strokes: builder not idle".into());
        }
        let mut seen = std::collections::HashSet::new();
        for s in &self.committed {
            seen.insert(s.id.as_str().to_string());
        }
        for s in &strokes {
            if !s.finished {
                return Err(format!("refuse unfinished stroke {}", s.id));
            }
            if s.points.is_empty() {
                return Err(format!("refuse empty stroke {}", s.id));
            }
            if !seen.insert(s.id.as_str().to_string()) {
                return Err(format!("duplicate stroke id {}", s.id));
            }
        }
        for s in strokes {
            let id = s.id.as_str().to_string();
            let root = root_of.get(&id).cloned().unwrap_or_else(|| id.clone());
            self.root_of.insert(id, root);
            self.committed.push(s);
        }
        Ok(())
    }

    pub fn root_of_map(&self) -> &HashMap<String, String> {
        &self.root_of
    }

    /// Root lineage id for a visible stroke (defaults to own id).
    pub fn root_id_of(&self, stroke_id: &StrokeId) -> String {
        self.root_of
            .get(stroke_id.as_str())
            .cloned()
            .unwrap_or_else(|| stroke_id.as_str().to_string())
    }

    /// All currently visible stroke ids sharing `root`, in render order.
    pub fn visible_ids_for_root(&self, root: &str) -> Vec<StrokeId> {
        self.committed
            .iter()
            .filter(|s| self.root_id_of(&s.id) == root)
            .map(|s| s.id.clone())
            .collect()
    }

    /// Validate + prepare a replace against the current visible model (no mutation).
    pub fn prepare_replace_strokes(
        &self,
        replacements: &[crate::store::PersistedReplacementV2],
    ) -> Result<crate::store::PreparedReplace, String> {
        if self.phase != SessionPhase::Idle {
            return Err("prepare_replace_strokes requires Idle".into());
        }
        let mut known_ids: std::collections::HashSet<String> = self
            .committed
            .iter()
            .map(|s| s.id.as_str().to_string())
            .collect();
        for id in self.root_of.keys() {
            known_ids.insert(id.clone());
        }
        let model = crate::store::VisibleModel {
            strokes: self.committed.clone(),
            root_of: self.root_of.clone(),
            known_ids,
        };
        crate::store::prepare_replace_strokes(&model, replacements)
    }

    /// Install a previously prepared replace (after durable append/fsync succeeded).
    /// Infallible: preparation already performed all semantic checks.
    pub fn commit_prepared_replace(&mut self, prepared: crate::store::PreparedReplace) {
        debug_assert_eq!(self.phase, SessionPhase::Idle);
        self.committed = prepared.strokes;
        self.root_of = prepared.root_of;
    }

    /// Apply an in-memory replace (tests / helpers). Prefer prepare+commit for durable path.
    #[allow(dead_code)] // host tests; runtime uses prepare + commit_prepared_replace
    pub fn apply_replace_strokes(
        &mut self,
        replacements: &[crate::store::PersistedReplacementV2],
    ) -> Result<(), String> {
        let prepared = self.prepare_replace_strokes(replacements)?;
        self.commit_prepared_replace(prepared);
        Ok(())
    }

    /// Register root lineage for a newly committed live stroke (root = own id).
    pub fn register_committed_root(&mut self, stroke_id: &StrokeId) {
        let id = stroke_id.as_str().to_string();
        self.root_of.insert(id.clone(), id);
    }

    /// Idle-only: remove exactly one committed stroke by id. Preserves relative order.
    #[allow(dead_code)] // F3 path; F7 uses [`Self::apply_replace_strokes`]
    pub fn delete_committed_stroke(&mut self, stroke_id: &StrokeId) -> Result<(), String> {
        if self.phase != SessionPhase::Idle {
            return Err("delete_committed_stroke requires Idle".into());
        }
        if self.builder.is_active() {
            return Err("delete_committed_stroke: builder not idle".into());
        }
        let Some(pos) = self
            .committed
            .iter()
            .position(|s| s.id.as_str() == stroke_id.as_str())
        else {
            return Err(format!(
                "delete_committed_stroke: stroke {} not in visible model",
                stroke_id
            ));
        };
        self.committed.remove(pos);
        let op = NotebookOp::DeleteStroke {
            stroke_id: stroke_id.clone(),
        };
        self.ops.push(op);
        Ok(())
    }

    /// Set the `brush` style on a committed stroke by id (Gate F6 authoring tag).
    ///
    /// Called right after `on_pen_up` commits a live stroke, so the committed model reconstructs
    /// with the same style that is persisted. Refuses if the id is not the expected committed one.
    pub fn set_last_committed_brush(
        &mut self,
        stroke_id: &StrokeId,
        brush: Option<String>,
    ) -> Result<(), String> {
        let Some(stroke) = self
            .committed
            .iter_mut()
            .rev()
            .find(|s| s.id.as_str() == stroke_id.as_str())
        else {
            return Err(format!(
                "set_last_committed_brush: stroke {stroke_id} not in committed model"
            ));
        };
        stroke.brush = brush;
        Ok(())
    }

    /// Points in the active builder (0 when Idle).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn active_point_count(&self) -> usize {
        self.builder.current().map(|s| s.points.len()).unwrap_or(0)
    }

    /// Pen Down while Idle starts a stroke. Non-Idle returns `None` (no invented stroke).
    pub fn on_pen_down(&mut self, page: PagePoint, pressure: Option<f32>) -> Option<DownOutcome> {
        if self.phase != SessionPhase::Idle {
            return None;
        }
        let mut point = Point::at(page);
        if let Some(p) = pressure {
            point = point.with_pressure(p);
        }
        let outcome = self.builder.begin(point);
        // Duplicate Down while active would finish previous; F0 only calls from Idle.
        debug_assert!(outcome.implicitly_finished.is_none());
        self.phase = SessionPhase::WetStrokeActive;
        self.strokes_started += 1;
        let mut ops = Vec::new();
        let begin = NotebookOp::BeginStroke {
            page_id: self.page_id.clone(),
            stroke_id: outcome.stroke_id.clone(),
            point,
        };
        self.ops.push(begin.clone());
        ops.push(begin);
        Some(DownOutcome {
            stroke_id: outcome.stroke_id,
            point,
            ops,
        })
    }

    pub fn on_pen_move(&mut self, page: PagePoint, pressure: Option<f32>) -> Option<MoveOutcome> {
        if self.phase != SessionPhase::WetStrokeActive {
            return None;
        }
        let stroke_id = self.builder.current_id()?;
        let mut point = Point::at(page);
        if let Some(p) = pressure {
            point = point.with_pressure(p);
        }
        let appended = self.builder.append(point);
        let mut ops = Vec::new();
        if appended {
            let op = NotebookOp::AppendStrokePoints {
                stroke_id: stroke_id.clone(),
                points: vec![point],
            };
            self.ops.push(op.clone());
            ops.push(op);
        }
        Some(MoveOutcome {
            stroke_id,
            point,
            appended,
            ops,
        })
    }

    pub fn on_pen_up(&mut self, page: PagePoint, pressure: Option<f32>) -> Option<UpOutcome> {
        if self.phase != SessionPhase::WetStrokeActive {
            return None;
        }
        self.phase = SessionPhase::FinishingStroke;
        let mut point = Point::at(page);
        if let Some(p) = pressure {
            point = point.with_pressure(p);
        }
        let mut ops = Vec::new();
        if self.builder.append_if_moved(point) {
            if let Some(id) = self.builder.current_id() {
                let op = NotebookOp::AppendStrokePoints {
                    stroke_id: id,
                    points: vec![point],
                };
                self.ops.push(op.clone());
                ops.push(op);
            }
        }
        let stroke = self.builder.finish()?;
        let finish = NotebookOp::FinishStroke {
            stroke_id: stroke.id.clone(),
        };
        self.ops.push(finish.clone());
        ops.push(finish);
        self.committed.push(stroke.clone());
        self.strokes_committed += 1;
        self.phase = SessionPhase::Idle;
        debug_assert_eq!(self.builder.phase(), StrokePhase::Idle);
        Some(UpOutcome { stroke, ops })
    }

    /// Stream gap / cancel: abandon active stroke without committing.
    pub fn on_stream_gap(&mut self) -> GapOutcome {
        let mut ops = Vec::new();
        let cancelled = if self.phase == SessionPhase::WetStrokeActive
            || self.phase == SessionPhase::FinishingStroke
        {
            let stroke = self.builder.cancel();
            if let Some(ref s) = stroke {
                let op = NotebookOp::CancelStroke {
                    stroke_id: s.id.clone(),
                };
                self.ops.push(op.clone());
                ops.push(op);
                self.strokes_cancelled += 1;
            }
            stroke
        } else {
            None
        };
        self.phase = SessionPhase::Idle;
        GapOutcome { cancelled, ops }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f32, y: f32) -> PagePoint {
        PagePoint::new(x, y)
    }

    #[test]
    fn idle_down_activates() {
        let mut s = PageSession::new("p0");
        let d = s.on_pen_down(pt(1.0, 2.0), Some(0.4)).unwrap();
        assert_eq!(s.phase(), SessionPhase::WetStrokeActive);
        assert_eq!(s.strokes_started(), 1);
        assert!(matches!(d.ops[0], NotebookOp::BeginStroke { .. }));
    }

    #[test]
    fn active_move_appends_canonical_points() {
        let mut s = PageSession::new("p0");
        s.on_pen_down(pt(0.0, 0.0), None).unwrap();
        let m = s.on_pen_move(pt(10.0, 10.0), Some(0.5)).unwrap();
        assert!(m.appended);
        assert_eq!(s.active_point_count(), 2);
    }

    #[test]
    fn non_idle_down_does_not_start_second() {
        let mut s = PageSession::new("p0");
        s.on_pen_down(pt(0.0, 0.0), None).unwrap();
        assert!(s.on_pen_down(pt(9.0, 9.0), None).is_none());
        assert_eq!(s.strokes_started(), 1);
        assert_eq!(s.active_point_count(), 1);
    }

    #[test]
    fn up_commits_and_returns_idle() {
        let mut s = PageSession::new("p0");
        s.on_pen_down(pt(0.0, 0.0), None).unwrap();
        s.on_pen_move(pt(5.0, 5.0), None).unwrap();
        let u = s.on_pen_up(pt(6.0, 6.0), None).unwrap();
        assert!(u.stroke.finished);
        assert_eq!(s.phase(), SessionPhase::Idle);
        assert_eq!(s.committed_count(), 1);
        assert_eq!(s.strokes_committed(), 1);
    }

    #[test]
    fn second_stroke_preserves_first() {
        let mut s = PageSession::new("p0");
        s.on_pen_down(pt(0.0, 0.0), None).unwrap();
        s.on_pen_up(pt(1.0, 1.0), None).unwrap();
        let id1 = s.committed_strokes()[0].id.clone();
        s.on_pen_down(pt(20.0, 20.0), None).unwrap();
        s.on_pen_move(pt(30.0, 30.0), None).unwrap();
        s.on_pen_up(pt(31.0, 31.0), None).unwrap();
        assert_eq!(s.committed_count(), 2);
        assert_eq!(s.committed_strokes()[0].id, id1);
        assert_ne!(s.committed_strokes()[0].id, s.committed_strokes()[1].id);
    }

    #[test]
    fn stream_gap_cancels_without_commit() {
        let mut s = PageSession::new("p0");
        s.on_pen_down(pt(0.0, 0.0), None).unwrap();
        s.on_pen_move(pt(2.0, 2.0), None).unwrap();
        let g = s.on_stream_gap();
        assert!(g.cancelled.is_some());
        assert!(!g.cancelled.as_ref().unwrap().finished);
        assert_eq!(s.committed_count(), 0);
        assert_eq!(s.strokes_cancelled(), 1);
        assert_eq!(s.phase(), SessionPhase::Idle);
        assert!(matches!(g.ops[0], NotebookOp::CancelStroke { .. }));
    }

    #[test]
    fn move_while_idle_invents_nothing() {
        let mut s = PageSession::new("p0");
        assert!(s.on_pen_move(pt(1.0, 1.0), None).is_none());
        assert_eq!(s.committed_count(), 0);
    }

    #[test]
    fn up_while_idle_invents_nothing() {
        let mut s = PageSession::new("p0");
        assert!(s.on_pen_up(pt(1.0, 1.0), None).is_none());
    }

    #[test]
    fn load_committed_preserves_order_rejects_dup() {
        let mut s = PageSession::new("p0");
        let a = Stroke {
            id: StrokeId::new("s-a"),
            points: vec![Point::new(0.0, 0.0)],
            brush: None,
            finished: true,
        };
        let b = Stroke {
            id: StrokeId::new("s-b"),
            points: vec![Point::new(1.0, 1.0)],
            brush: None,
            finished: true,
        };
        s.load_committed_strokes(vec![a.clone(), b]).unwrap();
        assert_eq!(s.committed_count(), 2);
        assert_eq!(s.committed_strokes()[0].id.as_str(), "s-a");
        assert!(s
            .load_committed_strokes(vec![a])
            .unwrap_err()
            .contains("duplicate"));
    }

    #[test]
    fn rebind_replaces_committed_without_live_ops() {
        let mut s = PageSession::new("page-1");
        let a = Stroke {
            id: StrokeId::new("s-a"),
            points: vec![Point::new(0.0, 0.0)],
            brush: None,
            finished: true,
        };
        s.load_committed_strokes(vec![a]).unwrap();
        assert_eq!(s.page_id(), "page-1");
        let b = Stroke {
            id: StrokeId::new("s-b"),
            points: vec![Point::new(2.0, 2.0)],
            brush: None,
            finished: true,
        };
        s.rebind_page("page-2", vec![b]).unwrap();
        assert_eq!(s.page_id(), "page-2");
        assert_eq!(s.committed_count(), 1);
        assert_eq!(s.committed_strokes()[0].id.as_str(), "s-b");
        assert_eq!(s.strokes_committed(), 0);
        assert_eq!(s.ops.len(), 0);
    }

    #[test]
    fn rebind_refuses_when_wet() {
        let mut s = PageSession::new("page-1");
        s.on_pen_down(pt(0.0, 0.0), None).unwrap();
        assert!(s.rebind_page("page-2", vec![]).is_err());
    }
}
