//! Gate F7 provisional partial-stroke eraser gesture (commit-on-Up).
//!
//! During Down/Move only provisional in-memory geometry is clipped. No PageSession
//! mutation, no edits.jsonl write, no panel reconstruct until Eraser Up.

use std::collections::{HashMap, HashSet};

use notebook_core::{
    clip_stroke_against_eraser_sweep, new_stroke_id, PagePoint, Point, Stroke, StrokeId,
    ERASER_HIT_RADIUS_CANONICAL,
};

use crate::store::{PersistedFragmentV2, PersistedPointV1, PersistedReplacementV2};

/// Per-gesture counters (not persisted).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartialEraseGestureStats {
    pub sweeps: u64,
    pub sources_touched: u64,
    pub sources_fully_deleted: u64,
    pub fragments_created: u64,
    pub blank_sweeps: u64,
}

/// Working fragment during an active eraser gesture.
#[derive(Clone, Debug)]
pub struct WorkingFragment {
    pub stroke: Stroke,
    /// Visible stroke id at gesture Down that this fragment descends from.
    pub gesture_source_id: StrokeId,
    pub root_id: StrokeId,
}

/// Built on Eraser Up for durable persistence (or empty = no-op).
#[derive(Clone, Debug, Default)]
pub struct PartialEraseCommit {
    pub replacements: Vec<PersistedReplacementV2>,
    pub stats: PartialEraseGestureStats,
    /// True when geometry changed vs Down snapshot.
    pub dirty: bool,
}

#[derive(Clone, Debug)]
pub struct Active {
    /// Original visible strokes at Down (id → stroke).
    sources: HashMap<String, Stroke>,
    working: Vec<WorkingFragment>,
    last_page_point: PagePoint,
    touched_sources: HashSet<String>,
    stats: PartialEraseGestureStats,
    /// Every sweep actually clipped, in order (S2B: the semantic `EraseRegion` path).
    sweeps: Vec<(PagePoint, PagePoint)>,
    /// Lineage roots of the strokes visible at Down (S2B: `observed_roots`).
    observed_roots: Vec<String>,
}

/// The semantic erase intent of a gesture (S2B `EraseRegion`), in canonical page space.
#[derive(Clone, Debug, PartialEq)]
pub struct EraseIntent {
    /// Down point followed by each subsequent sample: the reducer's sweep sequence
    /// `(p0,p0), (p0,p1), (p1,p2), …` reproduces exactly the sweeps this gesture clipped.
    pub path: Vec<PagePoint>,
    /// Local lineage roots of every stroke visible at Down (sorted, unique).
    pub observed_roots: Vec<String>,
}

/// A fresh random local stroke ID for an erase fragment (unsynced erase).
pub fn new_fragment_local_id() -> String {
    new_stroke_id().as_str().to_string()
}

/// Provisional partial-eraser contact machine.
#[derive(Clone, Debug, Default)]
pub enum EraserGestureState {
    #[default]
    Idle,
    Active(Box<Active>),
}

impl EraserGestureState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active(_))
    }

    /// Snapshot visible strokes (+ lineage) and begin provisional gesture.
    pub fn on_down(
        &mut self,
        page: PagePoint,
        visible: &[Stroke],
        root_of: &HashMap<String, String>,
    ) -> bool {
        if self.is_active() {
            return false;
        }
        let mut sources = HashMap::new();
        let mut working = Vec::with_capacity(visible.len());
        let mut observed: Vec<String> = Vec::new();
        for s in visible {
            let id = s.id.as_str().to_string();
            sources.insert(id.clone(), s.clone());
            let root = root_of.get(&id).cloned().unwrap_or_else(|| id.clone());
            observed.push(root.clone());
            working.push(WorkingFragment {
                stroke: s.clone(),
                gesture_source_id: s.id.clone(),
                root_id: StrokeId::new(root),
            });
        }
        observed.sort();
        observed.dedup();
        *self = Self::Active(Box::new(Active {
            sources,
            working,
            last_page_point: page,
            touched_sources: HashSet::new(),
            stats: PartialEraseGestureStats::default(),
            sweeps: Vec::new(),
            observed_roots: observed,
        }));
        true
    }

    /// Clip working geometry against sweep `last → page`. No durable IO.
    pub fn process_to(&mut self, page: PagePoint) {
        let Self::Active(active) = self else {
            return;
        };
        let start = Point::at(active.last_page_point);
        let end = Point::at(page);
        active.sweeps.push((active.last_page_point, page));
        active.last_page_point = page;
        active.stats.sweeps += 1;

        let prev = std::mem::take(&mut active.working);
        let mut next = Vec::with_capacity(prev.len());
        let mut any_clip = false;
        for frag in prev {
            let clipped = clip_stroke_against_eraser_sweep(
                &frag.stroke,
                start,
                end,
                ERASER_HIT_RADIUS_CANONICAL,
            );
            if clipped.len() != 1
                || clipped[0].points.len() != frag.stroke.points.len()
                || points_differ(&clipped[0].points, &frag.stroke.points)
            {
                any_clip = true;
                active
                    .touched_sources
                    .insert(frag.gesture_source_id.as_str().to_string());
            }
            for mut piece in clipped {
                // Temporary id; durable IDs assigned on Up.
                piece.id = StrokeId::new("pending");
                next.push(WorkingFragment {
                    stroke: piece,
                    gesture_source_id: frag.gesture_source_id.clone(),
                    root_id: frag.root_id.clone(),
                });
            }
        }
        active.working = next;
        if !any_clip {
            active.stats.blank_sweeps += 1;
        }
    }

    /// Build the durable replace_strokes payload; clears gesture state.
    pub fn on_up(&mut self) -> PartialEraseCommit {
        self.on_up_with(&mut |_, _| new_fragment_local_id())
    }

    /// Like [`Self::on_up`], but fragment IDs come from `fragment_id(source_local_id, index)`
    /// (S2B: deterministic IDs matching the reducer). Index counts the source's surviving
    /// pieces in working order.
    pub fn on_up_with(
        &mut self,
        fragment_id: &mut dyn FnMut(&str, usize) -> String,
    ) -> PartialEraseCommit {
        match std::mem::take(self) {
            Self::Idle => PartialEraseCommit::default(),
            Self::Active(active) => build_commit(*active, fragment_id),
        }
    }

    /// The semantic intent of the active gesture, if its sweep sequence has the canonical
    /// shape (first sweep stationary at the Down point, each next sweep continuing the last).
    pub fn intent(&self) -> Option<EraseIntent> {
        let Self::Active(a) = self else {
            return None;
        };
        let (first, _) = a.sweeps.first()?;
        let mut path = vec![*first];
        for (i, (from, to)) in a.sweeps.iter().enumerate() {
            let expected_from = if i == 0 {
                *first
            } else {
                *path.last().unwrap()
            };
            if *from != expected_from || (i == 0 && to != first) {
                return None;
            }
            if i > 0 {
                path.push(*to);
            }
        }
        Some(EraseIntent {
            path,
            observed_roots: a.observed_roots.clone(),
        })
    }

    /// Discard provisional gesture (StreamGap / Ctrl+C).
    pub fn clear_on_stream_gap(&mut self) -> PartialEraseCommit {
        let _ = std::mem::take(self);
        PartialEraseCommit::default()
    }

    /// Working geometry for tests / diagnostics (Active only).
    #[allow(dead_code)]
    pub fn working_strokes(&self) -> Vec<Stroke> {
        match self {
            Self::Idle => Vec::new(),
            Self::Active(a) => a.working.iter().map(|w| w.stroke.clone()).collect(),
        }
    }
}

fn points_differ(a: &[Point], b: &[Point]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    for (p, q) in a.iter().zip(b.iter()) {
        if (p.x - q.x).abs() > f32::EPSILON || (p.y - q.y).abs() > f32::EPSILON {
            return true;
        }
    }
    false
}

fn build_commit(
    active: Active,
    fragment_id: &mut dyn FnMut(&str, usize) -> String,
) -> PartialEraseCommit {
    let mut stats = active.stats;
    stats.sources_touched = active.touched_sources.len() as u64;
    if active.touched_sources.is_empty() {
        return PartialEraseCommit {
            replacements: Vec::new(),
            stats,
            dirty: false,
        };
    }

    let mut replacements = Vec::new();
    let mut fragments_created = 0u64;
    let mut fully_deleted = 0u64;

    // Preserve source order as in original snapshot (HashMap iteration is unstable —
    // walk original working order of unique sources by first appearance in sources map
    // via sorted keys for determinism, but render-slot uses session apply by target index).
    let mut source_ids: Vec<String> = active.touched_sources.iter().cloned().collect();
    source_ids.sort();

    for sid in source_ids {
        let Some(original) = active.sources.get(&sid) else {
            continue;
        };
        // Final descendants for this gesture source, in working order.
        let descendants: Vec<&WorkingFragment> = active
            .working
            .iter()
            .filter(|w| w.gesture_source_id.as_str() == sid)
            .collect();

        // Compare to original: if single fragment equal to original geometry → skip.
        if descendants.len() == 1
            && !points_differ(&descendants[0].stroke.points, &original.points)
            && descendants[0].stroke.brush == original.brush
        {
            continue;
        }

        if descendants.is_empty() {
            fully_deleted += 1;
        }

        let mut fragments = Vec::new();
        for (i, d) in descendants.iter().enumerate() {
            let id = fragment_id(&sid, i);
            fragments_created += 1;
            fragments.push(stroke_to_persisted_fragment(&d.stroke, &id));
        }
        replacements.push(PersistedReplacementV2 {
            target_stroke_id: sid,
            fragments,
        });
    }

    stats.fragments_created = fragments_created;
    stats.sources_fully_deleted = fully_deleted;
    let dirty = !replacements.is_empty();
    PartialEraseCommit {
        replacements,
        stats,
        dirty,
    }
}

fn stroke_to_persisted_fragment(stroke: &Stroke, id: &str) -> PersistedFragmentV2 {
    let style = stroke.brush.as_deref().map(|s| s.to_string());
    PersistedFragmentV2 {
        id: id.to_string(),
        style,
        points: stroke
            .points
            .iter()
            .map(|p| PersistedPointV1 {
                x: p.x,
                y: p.y,
                pressure: p.pressure,
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// F5 name aliases retained for older call-site docs; prefer PartialErase* types.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub type EraserGestureStats = PartialEraseGestureStats;
#[allow(dead_code)]
pub type EraserGestureSummary = PartialEraseCommit;

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(id: &str, pts: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: pts.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            brush: None,
            finished: true,
        }
    }

    #[test]
    fn provisional_middle_erase_commits_two_fragments() {
        let s = stroke("s-a", &[(0.0, 0.0), (2000.0, 0.0)]);
        let mut roots = HashMap::new();
        roots.insert("s-a".into(), "s-a".into());
        let mut g = EraserGestureState::Idle;
        assert!(g.on_down(PagePoint::new(1000.0, -500.0), &[s], &roots));
        g.process_to(PagePoint::new(1000.0, 500.0));
        let commit = g.on_up();
        assert!(commit.dirty);
        assert_eq!(commit.replacements.len(), 1);
        assert_eq!(commit.replacements[0].fragments.len(), 2);
        assert!(!g.is_active());
    }

    #[test]
    fn blank_gesture_no_commit() {
        let s = stroke("s-a", &[(0.0, 0.0), (2000.0, 0.0)]);
        let mut roots = HashMap::new();
        roots.insert("s-a".into(), "s-a".into());
        let mut g = EraserGestureState::Idle;
        assert!(g.on_down(
            PagePoint::new(0.0, ERASER_HIT_RADIUS_CANONICAL + 100.0),
            &[s],
            &roots
        ));
        g.process_to(PagePoint::new(2000.0, ERASER_HIT_RADIUS_CANONICAL + 100.0));
        let commit = g.on_up();
        assert!(!commit.dirty);
        assert!(commit.replacements.is_empty());
    }

    #[test]
    fn stream_gap_discards() {
        let s = stroke("s-a", &[(0.0, 0.0), (2000.0, 0.0)]);
        let mut roots = HashMap::new();
        roots.insert("s-a".into(), "s-a".into());
        let mut g = EraserGestureState::Idle;
        assert!(g.on_down(PagePoint::new(1000.0, 0.0), &[s], &roots));
        g.process_to(PagePoint::new(1000.0, 100.0));
        let _ = g.clear_on_stream_gap();
        assert!(!g.is_active());
    }
}
