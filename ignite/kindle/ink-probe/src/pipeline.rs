//! Application layer: stylus events -> canonical strokes + operations -> display refresh.
//!
//! # Coordinate stages
//!
//! `StylusEvent.device` (device axis units) -> [`DeviceCalibration`] -> canonical page
//! coordinates, which is what a [`Point`], a [`notebook_core::Stroke`] and a [`NotebookOp`]
//! store -> [`ViewportTransform`] -> display pixels, which is all the [`DisplaySurface`] sees.
//! The probe uses [`ViewportTransform::identity`], but nothing below the viewport stage knows
//! the panel resolution.
//!
//! # Malformed-sequence policy
//!
//! * **Move without contact is dropped and counted** (`moves_dropped_without_contact`). The
//!   pipeline waits for the next `Down`. Synthesising a stroke from a mid-air move
//!   would invent a start point we never observed, and would put a stroke in the builder that
//!   the operation log did not record.
//! * **Down while a stroke is active** finishes the previous stroke first, and the implicitly
//!   finished stroke is emitted as an explicit `FinishStroke` operation before the new
//!   `BeginStroke`. A stroke can never vanish from the operation stream.
//! * **Up without contact** emits nothing.
//! * **Stream gap** (`SourcePoll::StreamGap`, typically after evdev `SYN_DROPPED`): any active
//!   stroke is cancelled (`CancelStroke`). No Down/Up is invented. Drawing resumes only after a
//!   fresh `Down`.
//!
//! The invariant these enforce: the `StrokeBuilder` state and the `NotebookOp` history always
//! agree, and replaying the operations reproduces the finished strokes exactly.

use kindle_platform::{
    DisplaySurface, MonotonicClock, PlatformResult, RefreshMode, SourceMode, SourcePoll,
    StylusEvent, StylusEventKind, StylusSource,
};
use notebook_core::{
    DeviceCalibration, NotebookOp, Point, StrokeBuilder, StrokeId, ViewportTransform,
};

use crate::metrics::{DispatchAvailability, LatencyStats};

/// Outcome of one [`InkPipeline::pump`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PumpOutcome {
    /// The source is permanently exhausted.
    Ended,
    /// Nothing available right now. The source is still live; poll again later.
    Idle,
    /// The per-call event budget was used up; more may be waiting.
    BudgetExhausted,
}

pub struct InkPipeline<D: DisplaySurface, C: MonotonicClock> {
    /// Stage 1: device axis units -> canonical page coordinates.
    pub calibration: DeviceCalibration,
    /// Stage 2: canonical page coordinates -> display pixels.
    pub viewport: ViewportTransform,
    pub strokes: StrokeBuilder,
    pub display: D,
    pub clock: C,
    pub stats: LatencyStats,
    pub ops: Vec<NotebookOp>,
    page_id: String,
    /// Last drawn location, in **display pixels**.
    last_display: Option<(f32, f32)>,
}

impl<D: DisplaySurface, C: MonotonicClock> InkPipeline<D, C> {
    pub fn new(
        display: D,
        calibration: DeviceCalibration,
        viewport: ViewportTransform,
        clock: C,
    ) -> Self {
        Self {
            calibration,
            viewport,
            strokes: StrokeBuilder::new(),
            display,
            clock,
            stats: LatencyStats::default(),
            ops: Vec::new(),
            page_id: "probe-page".to_string(),
            last_display: None,
        }
    }

    pub fn with_page_id(mut self, page_id: impl Into<String>) -> Self {
        self.page_id = page_id.into();
        self
    }

    pub fn handle_event(&mut self, ev: StylusEvent) -> PlatformResult<()> {
        self.stats.events += 1;
        let enter_us = self.clock.now_us();
        self.record_dispatch(&ev, enter_us);

        let page = self.calibration.to_page(ev.device);
        let mut point = Point::at(page);
        if let Some(p) = ev.pressure {
            point = point.with_pressure(p);
        }
        // Point.t_us is monotonic-only; never copy LinuxRealtime / Unknown into it.
        if let Some(t) = ev.kernel_t_us() {
            if ev.kernel_clock.may_populate_point_timestamp() {
                point = point.with_time(t);
            }
        }
        let display_pt = self.viewport.to_display(page);
        let dxy = (display_pt.x, display_pt.y);

        match ev.kind {
            StylusEventKind::Hover => {
                // Proximity without contact must never draw and must never open a stroke.
                self.stats.hovers_ignored += 1;
            }
            StylusEventKind::Down => {
                let outcome = self.strokes.begin(point);
                if let Some(orphan) = outcome.implicitly_finished {
                    // A duplicate Down implicitly closed a stroke: record it explicitly so the
                    // operation stream still contains the whole stroke.
                    self.ops.push(NotebookOp::FinishStroke {
                        stroke_id: orphan.id,
                    });
                    self.stats.strokes_finished += 1;
                }
                self.ops.push(NotebookOp::BeginStroke {
                    page_id: self.page_id.clone(),
                    stroke_id: outcome.stroke_id,
                    point,
                });
                self.last_display = Some(dxy);
            }
            StylusEventKind::Move => {
                if !self.strokes.is_active() {
                    // Deterministic recovery: drop. No stroke, no operation, no ink.
                    self.stats.moves_dropped_without_contact += 1;
                    return Ok(());
                }
                let appended = self.strokes.append(point);
                debug_assert!(appended, "append must succeed while a stroke is active");
                let stroke_id = self.current_stroke_id();
                self.ops.push(NotebookOp::AppendStrokePoints {
                    stroke_id,
                    points: vec![point],
                });
                self.draw_segment_to(dxy)?;
                self.last_display = Some(dxy);
            }
            StylusEventKind::Up => {
                if !self.strokes.is_active() {
                    self.last_display = None;
                    return Ok(());
                }
                // The Up coordinate is part of the rendered geometry, so it must also be part of
                // the canonical stroke - unless it repeats the last move, which would add a
                // duplicate vertex.
                if self.strokes.append_if_moved(point) {
                    let stroke_id = self.current_stroke_id();
                    self.ops.push(NotebookOp::AppendStrokePoints {
                        stroke_id,
                        points: vec![point],
                    });
                    self.draw_segment_to(dxy)?;
                }
                if let Some(stroke) = self.strokes.finish() {
                    self.ops.push(NotebookOp::FinishStroke {
                        stroke_id: stroke.id,
                    });
                    self.stats.strokes_finished += 1;
                }
                self.last_display = None;
            }
        }
        Ok(())
    }

    fn current_stroke_id(&self) -> StrokeId {
        self.strokes
            .current_id()
            .expect("a stroke is active in this branch")
    }

    /// Draw from the last display position to `to`, timing draw and refresh separately.
    fn draw_segment_to(&mut self, to: (f32, f32)) -> PlatformResult<()> {
        let Some(from) = self.last_display else {
            return Ok(());
        };
        let t0 = self.clock.now_us();
        let dirty = self.display.draw_polyline_black(&[from, to])?;
        let t1 = self.clock.now_us();
        self.stats.draw.record(t1.saturating_sub(t0));

        if let Some(region) = dirty {
            self.display.refresh(region, RefreshMode::Partial)?;
            let t2 = self.clock.now_us();
            self.stats.refresh_submit.record(t2.saturating_sub(t1));
            self.stats.draw_plus_submit.record(t2.saturating_sub(t0));
            self.stats.refreshes += 1;
        }
        Ok(())
    }

    /// Record dispatch latency only when the two timestamps are provably the same clock.
    fn record_dispatch(&mut self, ev: &StylusEvent, enter_us: u64) {
        let local = self.clock.domain();
        match ev.kernel_t_us() {
            None => self
                .stats
                .note_dispatch_unavailable(DispatchAvailability::NoKernelTimestamps),
            Some(kernel_us) => {
                if ev.kernel_clock.comparable_with(local) {
                    self.stats
                        .dispatch
                        .record(enter_us.saturating_sub(kernel_us));
                    self.stats.note_dispatch_available();
                } else {
                    self.stats.note_dispatch_unavailable(
                        DispatchAvailability::ClockDomainMismatch {
                            kernel: ev.kernel_clock,
                            local,
                        },
                    );
                }
            }
        }
    }

    /// Process up to `budget` events.
    ///
    /// Never treats "nothing available right now" as end of stream: a live source that has gone
    /// quiet returns [`PumpOutcome::Idle`] and stays usable.
    pub fn pump<S: StylusSource>(
        &mut self,
        source: &mut S,
        budget: usize,
    ) -> PlatformResult<PumpOutcome> {
        for _ in 0..budget {
            match source.poll_event()? {
                SourcePoll::Event(ev) => self.handle_event(ev)?,
                SourcePoll::StreamGap { had_active_contact } => {
                    self.handle_stream_gap(had_active_contact)?
                }
                SourcePoll::WouldBlock => return Ok(PumpOutcome::Idle),
                SourcePoll::EndOfStream => return Ok(PumpOutcome::Ended),
            }
        }
        Ok(PumpOutcome::BudgetExhausted)
    }

    /// Fail-closed response to an input stream gap: cancel any active stroke; invent nothing.
    fn handle_stream_gap(&mut self, had_active_contact: bool) -> PlatformResult<()> {
        self.stats.stream_gaps += 1;
        if had_active_contact || self.strokes.is_active() {
            if let Some(stroke) = self.strokes.cancel() {
                self.ops.push(NotebookOp::CancelStroke {
                    stroke_id: stroke.id,
                });
                self.stats.strokes_cancelled += 1;
            }
        }
        self.last_display = None;
        Ok(())
    }

    /// Pump until the source ends or goes idle.
    ///
    /// Returns [`PumpOutcome::Ended`] only on a genuine end of stream. For a blocking source
    /// ([`SourceMode::Blocking`]) that is the only way this returns, which is what a live probe
    /// wants; for a non-blocking source it also returns on [`PumpOutcome::Idle`] so the caller
    /// can wait rather than spin.
    pub fn run<S: StylusSource>(&mut self, source: &mut S) -> PlatformResult<PumpOutcome> {
        loop {
            match self.pump(source, 256)? {
                PumpOutcome::BudgetExhausted => continue,
                PumpOutcome::Idle if source.mode() == SourceMode::Blocking => continue,
                other => return Ok(other),
            }
        }
    }

    /// Replay [`InkPipeline::ops`] into stroke geometry, for consistency checks.
    pub fn replay_ops(&self) -> Vec<(StrokeId, Vec<Point>, bool)> {
        let mut out: Vec<(StrokeId, Vec<Point>, bool)> = Vec::new();
        for op in &self.ops {
            match op {
                NotebookOp::BeginStroke {
                    stroke_id, point, ..
                } => out.push((stroke_id.clone(), vec![*point], false)),
                NotebookOp::AppendStrokePoints { stroke_id, points } => {
                    if let Some(e) = out.iter_mut().find(|e| &e.0 == stroke_id) {
                        e.1.extend(points.iter().copied());
                    }
                }
                NotebookOp::FinishStroke { stroke_id } => {
                    if let Some(e) = out.iter_mut().find(|e| &e.0 == stroke_id) {
                        e.2 = true;
                    }
                }
                NotebookOp::CancelStroke { stroke_id } => {
                    // Cancelled strokes remain unfinished and are dropped from the finished set.
                    out.retain(|(id, _, _)| id != stroke_id);
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kindle_platform::host::{HostDisplay, LiveScriptedStylus, ManualClock, ScriptedStylus};
    use kindle_platform::{ClockDomain, HostClock, StylusTool};
    use notebook_core::{Orientation, PageGeometry};

    fn page() -> PageGeometry {
        PageGeometry::new(200.0, 200.0)
    }

    fn calibration() -> DeviceCalibration {
        DeviceCalibration {
            device_min_x: 0.0,
            device_max_x: 100.0,
            device_min_y: 0.0,
            device_max_y: 100.0,
            orientation: Orientation::Portrait0,
            page: page(),
        }
    }

    fn pipeline() -> InkPipeline<HostDisplay, HostClock> {
        InkPipeline::new(
            HostDisplay::new(200, 200),
            calibration(),
            ViewportTransform::identity(),
            HostClock::new(),
        )
    }

    fn ev(kind: StylusEventKind, x: f32, y: f32) -> StylusEvent {
        StylusEvent::new(kind, x, y)
    }

    fn demo_events() -> Vec<StylusEvent> {
        vec![
            ev(StylusEventKind::Down, 10.0, 10.0).with_pressure(0.4),
            ev(StylusEventKind::Move, 20.0, 15.0).with_pressure(0.5),
            ev(StylusEventKind::Move, 40.0, 30.0).with_pressure(0.6),
            ev(StylusEventKind::Up, 50.0, 35.0).with_pressure(0.1),
        ]
    }

    #[test]
    fn pipeline_draws_and_records_ops() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(demo_events());
        assert_eq!(p.run(&mut src).unwrap(), PumpOutcome::Ended);
        assert_eq!(p.stats.strokes_finished, 1);
        assert!(p.stats.refreshes >= 1);
        assert!(p.display.inked_pixel_count() > 0);
        assert!(p
            .ops
            .iter()
            .any(|o| matches!(o, NotebookOp::FinishStroke { .. })));
    }

    // --- Rendered stroke vs canonical stroke -------------------------------------------------

    #[test]
    fn up_coordinate_is_part_of_the_canonical_stroke() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(demo_events());
        p.run(&mut src).unwrap();
        let stroke = &p.strokes.finished_strokes()[0];
        // Down + 2 moves + the distinct Up location.
        assert_eq!(stroke.points.len(), 4);
        let last = stroke.points.last().unwrap();
        // Device (50,35) over a 0..100 axis onto a 200x200 page.
        assert_eq!((last.x, last.y), (100.0, 70.0));
    }

    #[test]
    fn up_at_the_last_move_location_does_not_duplicate_the_final_point() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(vec![
            ev(StylusEventKind::Down, 10.0, 10.0),
            ev(StylusEventKind::Move, 20.0, 20.0),
            // Pen lifted without moving: same coordinate, zero pressure.
            ev(StylusEventKind::Up, 20.0, 20.0).with_pressure(0.0),
        ]);
        p.run(&mut src).unwrap();
        let stroke = &p.strokes.finished_strokes()[0];
        assert_eq!(stroke.points.len(), 2);
        assert_eq!((stroke.points[1].x, stroke.points[1].y), (40.0, 40.0));
    }

    #[test]
    fn rendered_geometry_equals_the_canonical_stroke() {
        // Every segment handed to the display must correspond to a stored vertex pair.
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(demo_events());
        p.run(&mut src).unwrap();
        let stroke = &p.strokes.finished_strokes()[0];
        // One refresh per drawn segment, and a polyline of n points has n-1 segments.
        assert_eq!(p.stats.refreshes as usize, stroke.points.len() - 1);
    }

    #[test]
    fn ops_replay_reproduces_the_finished_stroke_exactly() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(demo_events());
        p.run(&mut src).unwrap();
        let replayed = p.replay_ops();
        assert_eq!(replayed.len(), 1);
        let (id, points, finished) = &replayed[0];
        let stroke = &p.strokes.finished_strokes()[0];
        assert_eq!(id, &stroke.id);
        assert!(finished);
        assert_eq!(points, &stroke.points);
    }

    // --- Malformed sequences ----------------------------------------------------------------

    #[test]
    fn move_without_contact_is_dropped_and_leaves_no_trace() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(vec![
            ev(StylusEventKind::Move, 10.0, 10.0),
            ev(StylusEventKind::Move, 20.0, 20.0),
        ]);
        p.run(&mut src).unwrap();
        assert_eq!(p.stats.moves_dropped_without_contact, 2);
        assert!(p.ops.is_empty());
        assert!(p.strokes.current().is_none());
        assert!(p.strokes.finished_strokes().is_empty());
        assert_eq!(p.display.inked_pixel_count(), 0);
    }

    #[test]
    fn pipeline_recovers_on_the_next_down_after_dropped_moves() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(vec![
            ev(StylusEventKind::Move, 90.0, 90.0),
            ev(StylusEventKind::Down, 10.0, 10.0),
            ev(StylusEventKind::Move, 20.0, 20.0),
            ev(StylusEventKind::Up, 30.0, 30.0),
        ]);
        p.run(&mut src).unwrap();
        assert_eq!(p.stats.moves_dropped_without_contact, 1);
        assert_eq!(p.strokes.finished_strokes().len(), 1);
        assert_eq!(p.strokes.finished_strokes()[0].points.len(), 3);
    }

    #[test]
    fn stream_gap_cancels_active_stroke_and_requires_fresh_down() {
        use kindle_platform::host::ScriptedPolls;
        use kindle_platform::SourcePoll;

        let mut p = pipeline();
        let mut src = ScriptedPolls::new(vec![
            SourcePoll::Event(ev(StylusEventKind::Down, 10.0, 10.0)),
            SourcePoll::Event(ev(StylusEventKind::Move, 20.0, 20.0)),
            SourcePoll::StreamGap {
                had_active_contact: true,
            },
            // Motion after the gap must not bridge the cancelled stroke.
            SourcePoll::Event(ev(StylusEventKind::Move, 50.0, 50.0)),
            SourcePoll::Event(ev(StylusEventKind::Down, 60.0, 60.0)),
            SourcePoll::Event(ev(StylusEventKind::Move, 70.0, 70.0)),
            SourcePoll::Event(ev(StylusEventKind::Up, 80.0, 80.0)),
        ]);
        p.run(&mut src).unwrap();

        assert_eq!(p.stats.stream_gaps, 1);
        assert_eq!(p.stats.strokes_cancelled, 1);
        assert_eq!(p.stats.moves_dropped_without_contact, 1);
        assert_eq!(p.stats.strokes_finished, 1);
        assert_eq!(p.strokes.finished_strokes().len(), 1);
        assert!(p
            .ops
            .iter()
            .any(|o| matches!(o, NotebookOp::CancelStroke { .. })));
        // Cancelled stroke is absent from replay finished set.
        assert_eq!(p.replay_ops().len(), 1);
    }

    #[test]
    fn stream_gap_without_active_stroke_invents_nothing() {
        use kindle_platform::host::ScriptedPolls;
        use kindle_platform::SourcePoll;

        let mut p = pipeline();
        let mut src = ScriptedPolls::new(vec![
            SourcePoll::StreamGap {
                had_active_contact: false,
            },
            SourcePoll::Event(ev(StylusEventKind::Down, 5.0, 5.0)),
            SourcePoll::Event(ev(StylusEventKind::Up, 6.0, 6.0)),
        ]);
        p.run(&mut src).unwrap();
        assert_eq!(p.stats.stream_gaps, 1);
        assert_eq!(p.stats.strokes_cancelled, 0);
        assert_eq!(p.stats.strokes_finished, 1);
        assert!(!p
            .ops
            .iter()
            .any(|o| matches!(o, NotebookOp::CancelStroke { .. })));
    }

    #[test]
    fn duplicate_down_keeps_the_previous_stroke_in_the_operation_stream() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(vec![
            ev(StylusEventKind::Down, 10.0, 10.0),
            ev(StylusEventKind::Move, 20.0, 20.0),
            // Second Down with no intervening Up.
            ev(StylusEventKind::Down, 60.0, 60.0),
            ev(StylusEventKind::Move, 70.0, 70.0),
            ev(StylusEventKind::Up, 80.0, 80.0),
        ]);
        p.run(&mut src).unwrap();

        assert_eq!(p.strokes.finished_strokes().len(), 2);
        assert_eq!(p.stats.strokes_finished, 2);

        let begins: Vec<_> = p
            .ops
            .iter()
            .filter(|o| matches!(o, NotebookOp::BeginStroke { .. }))
            .collect();
        let finishes: Vec<_> = p
            .ops
            .iter()
            .filter(|o| matches!(o, NotebookOp::FinishStroke { .. }))
            .collect();
        assert_eq!(begins.len(), 2);
        assert_eq!(
            finishes.len(),
            2,
            "the orphaned stroke must be finished too"
        );

        // Replay must reproduce both strokes, including the implicitly finished one.
        let replayed = p.replay_ops();
        assert_eq!(replayed.len(), 2);
        for (i, (id, points, finished)) in replayed.iter().enumerate() {
            let stroke = &p.strokes.finished_strokes()[i];
            assert_eq!(id, &stroke.id);
            assert!(finished);
            assert_eq!(points, &stroke.points);
        }
    }

    #[test]
    fn up_without_down_emits_nothing() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(vec![ev(StylusEventKind::Up, 10.0, 10.0)]);
        p.run(&mut src).unwrap();
        assert!(p.ops.is_empty());
        assert_eq!(p.stats.strokes_finished, 0);
        assert_eq!(p.display.inked_pixel_count(), 0);
    }

    #[test]
    fn hover_never_draws_or_opens_a_stroke() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(vec![
            ev(StylusEventKind::Hover, 10.0, 10.0),
            ev(StylusEventKind::Hover, 20.0, 20.0).with_tool(StylusTool::Eraser),
            ev(StylusEventKind::Hover, 30.0, 30.0),
        ]);
        p.run(&mut src).unwrap();
        assert_eq!(p.stats.hovers_ignored, 3);
        assert!(p.ops.is_empty());
        assert_eq!(p.display.inked_pixel_count(), 0);
        assert_eq!(p.stats.refreshes, 0);
    }

    // --- Live source semantics --------------------------------------------------------------

    #[test]
    fn a_quiet_live_source_does_not_terminate_the_pipeline() {
        let mut p = pipeline();
        let mut src = LiveScriptedStylus::new(vec![
            ev(StylusEventKind::Down, 10.0, 10.0),
            ev(StylusEventKind::Move, 20.0, 20.0),
        ]);
        // First burst, then the user pauses.
        assert_eq!(p.run(&mut src).unwrap(), PumpOutcome::Idle);
        assert_eq!(p.stats.events, 2);
        assert!(p.strokes.is_active());

        // Polling an idle live source repeatedly must stay Idle, never Ended.
        for _ in 0..5 {
            assert_eq!(p.run(&mut src).unwrap(), PumpOutcome::Idle);
        }

        // The user writes again: the same stroke continues.
        src.push(ev(StylusEventKind::Move, 30.0, 30.0));
        src.push(ev(StylusEventKind::Up, 40.0, 40.0));
        assert_eq!(p.run(&mut src).unwrap(), PumpOutcome::Idle);
        assert_eq!(p.stats.strokes_finished, 1);
        assert_eq!(p.strokes.finished_strokes()[0].points.len(), 4);
    }

    #[test]
    fn a_live_source_ends_only_when_it_actually_closes() {
        let mut p = pipeline();
        let mut src = LiveScriptedStylus::new(vec![]);
        assert_eq!(p.run(&mut src).unwrap(), PumpOutcome::Idle);
        src.close();
        assert_eq!(p.run(&mut src).unwrap(), PumpOutcome::Ended);
    }

    #[test]
    fn a_finite_scripted_source_still_ends() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(demo_events());
        assert_eq!(p.run(&mut src).unwrap(), PumpOutcome::Ended);
    }

    #[test]
    fn pump_respects_its_budget() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(demo_events());
        assert_eq!(p.pump(&mut src, 2).unwrap(), PumpOutcome::BudgetExhausted);
        assert_eq!(p.stats.events, 2);
    }

    // --- Coordinate spaces ------------------------------------------------------------------

    #[test]
    fn stored_geometry_is_page_space_not_display_pixels() {
        // Page is 200x200 but the panel is 400x400 with a 2x viewport.
        let mut p = InkPipeline::new(
            HostDisplay::new(400, 400),
            calibration(),
            ViewportTransform::fit(page(), 400.0, 400.0),
            HostClock::new(),
        );
        let mut src = ScriptedStylus::new(vec![
            ev(StylusEventKind::Down, 50.0, 50.0),
            ev(StylusEventKind::Up, 100.0, 100.0),
        ]);
        p.run(&mut src).unwrap();
        let stroke = &p.strokes.finished_strokes()[0];
        // Device 50/100 -> page 100 (not 200 display pixels).
        assert_eq!((stroke.points[0].x, stroke.points[0].y), (100.0, 100.0));
        assert_eq!((stroke.points[1].x, stroke.points[1].y), (200.0, 200.0));
        // …while the ink landed at the doubled display coordinates.
        assert!(p.display.inked_pixel_count() > 0);
    }

    #[test]
    fn orientation_is_applied_before_page_space() {
        let mut cal = calibration();
        cal.orientation = Orientation::Portrait180;
        let mut p = InkPipeline::new(
            HostDisplay::new(200, 200),
            cal,
            ViewportTransform::identity(),
            HostClock::new(),
        );
        p.handle_event(ev(StylusEventKind::Down, 0.0, 0.0)).unwrap();
        let s = p.strokes.current().unwrap();
        assert_eq!((s.points[0].x, s.points[0].y), (200.0, 200.0));
    }

    #[test]
    fn strokes_outside_the_panel_do_not_panic_and_submit_no_refresh() {
        // Page larger than the surface: part of the stroke falls off the panel.
        let mut p = InkPipeline::new(
            HostDisplay::new(50, 50),
            calibration(),
            ViewportTransform::identity(),
            HostClock::new(),
        );
        let mut src = ScriptedStylus::new(vec![
            ev(StylusEventKind::Down, 95.0, 95.0),
            ev(StylusEventKind::Move, 99.0, 99.0),
            ev(StylusEventKind::Up, 100.0, 100.0),
        ]);
        p.run(&mut src).unwrap();
        // Geometry is still recorded in page space even though nothing was displayable.
        assert_eq!(p.strokes.finished_strokes()[0].points.len(), 3);
        for (region, _) in &p.display.refresh_log {
            assert!(region.is_within(50, 50));
        }
    }

    // --- Point timestamp domain policy ------------------------------------------------------

    #[test]
    fn monotonic_kernel_timestamps_reach_point_t_us() {
        let mut p = pipeline();
        p.handle_event(
            ev(StylusEventKind::Down, 10.0, 10.0)
                .with_kernel_time(42_000, ClockDomain::LinuxMonotonic),
        )
        .unwrap();
        match &p.ops[0] {
            NotebookOp::BeginStroke { point, .. } => assert_eq!(point.t_us, Some(42_000)),
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn synthetic_timestamps_reach_point_t_us_for_deterministic_tests() {
        let mut p = pipeline();
        p.handle_event(
            ev(StylusEventKind::Down, 10.0, 10.0).with_kernel_time(7, ClockDomain::Synthetic),
        )
        .unwrap();
        match &p.ops[0] {
            NotebookOp::BeginStroke { point, .. } => assert_eq!(point.t_us, Some(7)),
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn realtime_kernel_timestamps_do_not_populate_point_t_us() {
        let mut p = pipeline();
        p.handle_event(
            ev(StylusEventKind::Down, 10.0, 10.0)
                .with_kernel_time(99_000, ClockDomain::LinuxRealtime),
        )
        .unwrap();
        match &p.ops[0] {
            NotebookOp::BeginStroke { point, .. } => assert_eq!(point.t_us, None),
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn unknown_domain_timestamps_do_not_populate_point_t_us() {
        let mut p = pipeline();
        p.handle_event(
            ev(StylusEventKind::Down, 10.0, 10.0).with_kernel_time(1, ClockDomain::Unknown),
        )
        .unwrap();
        match &p.ops[0] {
            NotebookOp::BeginStroke { point, .. } => assert_eq!(point.t_us, None),
            other => panic!("unexpected op: {other:?}"),
        }
    }

    // --- Latency metrics --------------------------------------------------------------------

    #[test]
    fn dispatch_latency_is_unavailable_without_kernel_timestamps() {
        let mut p = pipeline();
        let mut src = ScriptedStylus::new(demo_events());
        p.run(&mut src).unwrap();
        assert!(p.stats.dispatch.summary().is_none());
        assert_eq!(
            p.stats.dispatch_availability(),
            DispatchAvailability::NoKernelTimestamps
        );
        assert!(p.stats.summary_line().contains("dispatch[unavailable"));
    }

    #[test]
    fn dispatch_latency_is_unavailable_across_clock_domains() {
        let mut p = InkPipeline::new(
            HostDisplay::new(200, 200),
            calibration(),
            ViewportTransform::identity(),
            HostClock::new(), // HostInstant
        );
        p.handle_event(
            ev(StylusEventKind::Down, 10.0, 10.0)
                .with_kernel_time(1_000, ClockDomain::LinuxRealtime),
        )
        .unwrap();
        assert!(p.stats.dispatch.summary().is_none());
        assert!(matches!(
            p.stats.dispatch_availability(),
            DispatchAvailability::ClockDomainMismatch { .. }
        ));
    }

    #[test]
    fn dispatch_latency_is_measured_within_one_clock_domain() {
        let clock = ManualClock::new(5_000);
        let mut p = InkPipeline::new(
            HostDisplay::new(200, 200),
            calibration(),
            ViewportTransform::identity(),
            clock,
        );
        p.handle_event(
            ev(StylusEventKind::Down, 10.0, 10.0).with_kernel_time(4_200, ClockDomain::Synthetic),
        )
        .unwrap();
        let s = p.stats.dispatch.summary().expect("measurable");
        assert_eq!(s.count, 1);
        assert_eq!(s.min_us, 800);
        assert_eq!(
            p.stats.dispatch_availability(),
            DispatchAvailability::Available
        );
    }

    #[test]
    fn draw_and_refresh_are_timed_separately() {
        // 10us per clock read, so each timed span is a known multiple.
        let clock = ManualClock::new(0).with_auto_advance(10);
        let mut p = InkPipeline::new(
            HostDisplay::new(200, 200),
            calibration(),
            ViewportTransform::identity(),
            clock,
        );
        let mut src = ScriptedStylus::new(vec![
            ev(StylusEventKind::Down, 10.0, 10.0),
            ev(StylusEventKind::Move, 20.0, 20.0),
        ]);
        p.run(&mut src).unwrap();

        let draw = p.stats.draw.summary().expect("draw timed");
        let submit = p.stats.refresh_submit.summary().expect("refresh timed");
        let total = p.stats.draw_plus_submit.summary().expect("total timed");
        assert_eq!(draw.count, 1);
        assert_eq!(submit.count, 1);
        assert_eq!(total.count, 1);
        assert_eq!(draw.mean_us, 10);
        assert_eq!(submit.mean_us, 10);
        assert_eq!(total.mean_us, 20);
        // The total is the sum of its parts, not an unrelated epoch subtraction.
        assert_eq!(total.mean_us, draw.mean_us + submit.mean_us);
    }

    #[test]
    fn metrics_are_named_rather_than_one_ambiguous_vector() {
        let line = LatencyStats::default().summary_line();
        for name in [
            "dispatch",
            "draw_us",
            "refresh_submit_us",
            "draw_plus_submit_us",
        ] {
            assert!(line.contains(name), "missing {name} in {line}");
        }
    }

    #[test]
    fn page_id_is_configurable_and_lands_in_the_begin_operation() {
        let mut p = pipeline().with_page_id("page-7");
        p.handle_event(ev(StylusEventKind::Down, 1.0, 1.0)).unwrap();
        match &p.ops[0] {
            NotebookOp::BeginStroke { page_id, .. } => assert_eq!(page_id, "page-7"),
            other => panic!("unexpected first op: {other:?}"),
        }
    }
}
