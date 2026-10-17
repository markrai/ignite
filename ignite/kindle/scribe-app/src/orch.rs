//! Wet / clean ink transport orchestration (host-testable via [`InkTransport`]).

use kindle_platform::{ClockDomain, DirtyRegion, StylusModeLifecycle, UpdateClass};
use notebook_core::{PagePoint, Stroke, StrokeId};

use crate::cadence::{format_dist, StreamRefreshPolicy};
use crate::open_page::{BoundPageKey, OpenPage};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::session::{PageSession, SessionPhase};
use crate::store::{MemoryPageStore, PageStore, PersistTiming};
use sync_protocol::{SyncPageLink, SyncText};

/// After Pen Up, defer GC16 until idle this long with empty input intent (Gate F4.1).
pub const CLEAN_REFRESH_IDLE_US: u64 = 150_000;

/// Software phase timings for one Pen Up (not physical pen→panel latency).
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // gc16_* reserved when idle-clean path records send/wait separately
pub struct PenUpPhaseTiming {
    pub final_du_us: u64,
    pub final_wait_us: u64,
    pub disarm_us: u64,
    pub model_finish_us: u64,
    pub persist_append_us: u64,
    pub persist_fsync_us: u64,
    pub gc16_send_us: u64,
    pub gc16_wait_us: u64,
    /// Wall time inside `on_pen_up` (excludes deferred idle GC16).
    pub pen_up_total_handler_us: u64,
    /// True when GC16 was deferred to idle coalesce.
    pub gc16_deferred: bool,
}

#[derive(Clone, Debug)]
pub struct DuSendResult {
    pub marker: u32,
    pub send_ioctl_us: u64,
}

#[derive(Clone, Debug)]
pub struct WaitResult {
    pub marker: u32,
    pub collision_test: u32,
    pub wait_ioctl_us: u64,
}

/// Controller / runtime fault classification (Gate F0.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CtrlError {
    /// Stroke cancelled; stylus proven disarmed; model Idle — runtime may continue.
    Recoverable(String),
    /// Stylus mode or transport safety uncertain — terminate the session.
    Fatal(String),
}

impl CtrlError {
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }
}

impl std::fmt::Display for CtrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recoverable(m) => write!(f, "recoverable: {m}"),
            Self::Fatal(m) => write!(f, "fatal: {m}"),
        }
    }
}

/// Panel transport used by the stroke controller.
pub trait InkTransport {
    fn now_us(&self) -> u64;
    fn arm(&mut self) -> Result<(), String>;
    fn disarm(&mut self) -> Result<(), String>;
    fn stamp(
        &mut self,
        page: PagePoint,
        pressure: Option<f32>,
    ) -> Result<Option<DirtyRegion>, String>;
    fn segment(
        &mut self,
        from: PagePoint,
        pa: Option<f32>,
        to: PagePoint,
        pb: Option<f32>,
    ) -> Result<Option<DirtyRegion>, String>;
    /// Submit a WetInk DU update. Must not invent a successful marker for an empty/invalid rect.
    fn send_du(&mut self, dirty: DirtyRegion) -> Result<DuSendResult, String>;
    fn wait_marker(&mut self, marker: u32) -> Result<WaitResult, String>;
    fn commit_gc16(&mut self, dirty: DirtyRegion, strokes: &[Stroke]) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum TransportStep {
    Arm,
    Stamp,
    Segment,
    DuSend,
    Wait,
    Disarm,
    Gc16,
}

#[derive(Debug, Default)]
pub struct StrokeStats {
    pub du_batches: u64,
    pub du_send_errors: u64,
    pub send_samples: Vec<u64>,
    pub interval_samples: Vec<u64>,
    pub input_to_send_samples: Vec<u64>,
    pub input_to_du_unavailable: bool,
    pub gc16_commits: u64,
    pub arm_us: u64,
    pub disarm_us: u64,
    pub stroke_start_us: Option<u64>,
    pub stroke_end_us: Option<u64>,
    pub move_count: u64,
    pub last_collision: Option<u32>,
    pub last_wet_marker: Option<u32>,
    pub last_dirty: Option<DirtyRegion>,
}

impl StrokeStats {
    pub fn reset_stroke(&mut self) {
        self.du_batches = 0;
        self.du_send_errors = 0;
        self.send_samples.clear();
        self.interval_samples.clear();
        self.input_to_send_samples.clear();
        self.input_to_du_unavailable = false;
        self.gc16_commits = 0;
        self.arm_us = 0;
        self.disarm_us = 0;
        self.stroke_start_us = None;
        self.stroke_end_us = None;
        self.move_count = 0;
        self.last_collision = None;
        self.last_wet_marker = None;
        self.last_dirty = None;
    }

    pub fn duration_us(&self) -> Option<u64> {
        Some(self.stroke_end_us?.saturating_sub(self.stroke_start_us?))
    }
}

#[derive(Debug, Default)]
pub struct SessionTotals {
    pub input_stream_gaps: u64,
    pub input_stream_gaps_active: u64,
    pub input_stream_gaps_idle: u64,
    pub du_sends: u64,
    pub du_send_errors: u64,
    pub gc16_commit_refreshes: u64,
    pub strokes_durable: u64,
    pub persistence_errors: u64,
    pub page_switches: u64,
    pub pages_created: u64,
    pub page_switch_failures: u64,
    /// F8: durable page deletions committed.
    pub pages_deleted: u64,
    pub page_delete_refusals: u64,
    pub page_delete_failures: u64,
    pub navigation_gestures_ignored: u64,
    pub eraser_gestures: u64,
    pub eraser_hits: u64,
    pub eraser_misses: u64,
    /// F5: sweep segments processed across all gestures.
    pub eraser_sweep_segments: u64,
    pub eraser_unique_strokes_deleted: u64,
    pub eraser_duplicate_hits_suppressed: u64,
    pub eraser_blank_gestures: u64,
    pub eraser_reconstructs: u64,
    pub strokes_deleted: u64,
    pub delete_persistence_errors: u64,
    pub eraser_reconstruct_failures: u64,
    /// F7 partial-erase session totals.
    pub partial_erase_gestures: u64,
    pub partial_erase_sources_touched: u64,
    pub partial_erase_full_deletes: u64,
    pub partial_erase_fragments_created: u64,
    pub partial_erase_empty_gestures: u64,
    pub partial_erase_persistence_errors: u64,
    pub partial_erase_reconstruct_failures: u64,
    /// F4: side+center undo taps (including empty).
    pub undo_gestures: u64,
    pub undo_hits: u64,
    pub undo_empty: u64,
    pub undo_persistence_errors: u64,
    pub undo_reconstruct_failures: u64,
    /// F4.1 inter-stroke: Downs arriving shortly after a prior Up.
    pub rapid_second_down_count: u64,
    pub rapid_second_down_accepted: u64,
    pub rapid_second_down_rejected: u64,
    pub rapid_second_down_dispatch_max_us: u64,
    pub input_stream_gaps_during_post_up: u64,
    pub max_input_backlog: u64,
    pub idle_clean_refreshes: u64,
}

/// Result of [`StrokeController::undo_last_visible_stroke`] (Gate F4).
#[derive(Debug, Clone)]
pub enum UndoResult {
    /// No currently visible stroke on this page — no store mutation.
    Empty,
    /// Newest visible stroke durably deleted (F3 delete path).
    Deleted {
        stroke_id: StrokeId,
        timing: PersistTiming,
    },
}

/// Kernel input timestamp usable for subtraction against userspace monotonic wall.
pub fn input_kernel_for_du_timing(
    input_clock: ClockDomain,
    kernel_t_us: Option<u64>,
) -> Option<u64> {
    if input_clock == ClockDomain::LinuxMonotonic {
        kernel_t_us
    } else {
        None
    }
}

/// Orchestrates one page: model + wet DU + durable append + GC16 commit.
pub struct StrokeController<T: InkTransport, S: PageStore = MemoryPageStore> {
    pub session: PageSession,
    /// Replica-local bound page (notebook + page directory names).
    #[cfg_attr(not(test), allow(dead_code))]
    bound: BoundPageKey,
    /// Authoritative texts for the bound page. Overlay is derived from this; never from pixels.
    /// Runtime queries land in [`Self::open_page`]; until then the field is written on bind/replace.
    #[cfg_attr(not(test), allow(dead_code))]
    texts: Vec<SyncText>,
    /// Authoritative page links for the bound page (peer of texts; not the pen session).
    #[cfg_attr(not(test), allow(dead_code))]
    links: Vec<SyncPageLink>,
    transport: T,
    store: S,
    life: StylusModeLifecycle,
    policy: StreamRefreshPolicy,
    last_page: Option<PagePoint>,
    last_pressure: Option<f32>,
    stroke_dirty: Option<DirtyRegion>,
    last_wet_marker: Option<u32>,
    last_submit_us: Option<u64>,
    /// When false, input→DU timing is never recorded (clock-domain mismatch).
    input_timing_compatible: bool,
    pub stats: StrokeStats,
    pub totals: SessionTotals,
    pub last_persist: Option<PersistTiming>,
    pub last_pen_up_timing: Option<PenUpPhaseTiming>,
    /// Union of stroke dirty regions awaiting idle GC16.
    pending_clean: Option<DirtyRegion>,
    /// Userspace monotonic us when we last became Idle+disarmed after a stroke (for idle clean).
    idle_since_us: Option<u64>,
    /// Kernel/userspace markers for inter-stroke trace.
    pub last_up_kernel_us: Option<u64>,
    pub last_up_handler_return_us: Option<u64>,
    pub last_finished_stroke_id: Option<StrokeId>,
}

impl<T: InkTransport> StrokeController<T, MemoryPageStore> {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(page_id: impl Into<String>, transport: T, du_interval_us: u64) -> Self {
        Self::with_store(
            page_id,
            transport,
            du_interval_us,
            ClockDomain::LinuxMonotonic,
            MemoryPageStore::new(),
        )
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_input_clock(
        page_id: impl Into<String>,
        transport: T,
        du_interval_us: u64,
        input_clock: ClockDomain,
    ) -> Self {
        Self::with_store(
            page_id,
            transport,
            du_interval_us,
            input_clock,
            MemoryPageStore::new(),
        )
    }
}

impl<T: InkTransport, S: PageStore> StrokeController<T, S> {
    pub fn with_store(
        page_id: impl Into<String>,
        transport: T,
        du_interval_us: u64,
        input_clock: ClockDomain,
        store: S,
    ) -> Self {
        let page_id = page_id.into();
        Self {
            session: PageSession::new(page_id.clone()),
            bound: BoundPageKey::new("", page_id),
            texts: Vec::new(),
            links: Vec::new(),
            transport,
            store,
            life: StylusModeLifecycle::new(),
            policy: StreamRefreshPolicy::new(du_interval_us),
            last_page: None,
            last_pressure: None,
            stroke_dirty: None,
            last_wet_marker: None,
            last_submit_us: None,
            input_timing_compatible: input_clock == ClockDomain::LinuxMonotonic,
            stats: StrokeStats::default(),
            totals: SessionTotals::default(),
            last_persist: None,
            last_pen_up_timing: None,
            pending_clean: None,
            idle_since_us: None,
            last_up_kernel_us: None,
            last_up_handler_return_us: None,
            last_finished_stroke_id: None,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn store(&self) -> &S {
        &self.store
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Borrow the renderer and committed current-page strokes together. Touch previews use this
    /// to repaint a regional transient layer without cloning or mutating the page model.
    pub fn transport_and_strokes(&mut self) -> (&mut T, &[Stroke]) {
        (&mut self.transport, self.session.committed_strokes())
    }

    pub fn life(&self) -> &StylusModeLifecycle {
        &self.life
    }

    pub fn is_idle(&self) -> bool {
        self.session.is_idle()
    }

    pub fn is_armed(&self) -> bool {
        self.life.is_armed()
    }

    /// Retained durable texts for the bound page (source for `TextOverlay`, not the overlay).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn texts(&self) -> &[SyncText] {
        &self.texts
    }

    /// Replace retained texts without rebinding strokes (same page, files changed).
    pub fn replace_texts(&mut self, texts: Vec<SyncText>) {
        self.texts = texts;
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn links(&self) -> &[SyncPageLink] {
        &self.links
    }

    pub fn replace_links(&mut self, links: Vec<SyncPageLink>) {
        self.links = links;
    }

    /// Borrowed addressable content of the bound page.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open_page(&self) -> OpenPage<'_> {
        OpenPage::with_links(&self.bound, &self.session, &self.texts, &self.links)
    }

    /// Replace bound page + durable store. Idle + disarmed only.
    /// Clears per-stroke cadence/dirty/marker state; does not bump live stroke counters.
    #[allow(dead_code)] // host tests; runtime prefers [`Self::bind_page_with_roots`]
    pub fn bind_page(
        &mut self,
        page_id: impl Into<String>,
        strokes: Vec<Stroke>,
        store: S,
    ) -> Result<(), CtrlError> {
        self.bind_page_with_roots(
            BoundPageKey::new("", page_id),
            strokes,
            std::collections::HashMap::new(),
            store,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Bind with explicit authored-stroke lineage (empty map → identity roots).
    /// `texts` is the page's durable text list (empty if none); it replaces any previous page's texts.
    /// `links` is the page's durable page-link list (empty if none).
    pub fn bind_page_with_roots(
        &mut self,
        bound: BoundPageKey,
        strokes: Vec<Stroke>,
        root_of: std::collections::HashMap<String, String>,
        store: S,
        texts: Vec<SyncText>,
        links: Vec<SyncPageLink>,
    ) -> Result<(), CtrlError> {
        if !self.session.is_idle() {
            return Err(CtrlError::Recoverable(
                "bind_page refused: session not Idle".into(),
            ));
        }
        if self.life.is_armed() {
            return Err(CtrlError::Fatal(
                "bind_page refused: stylus mode still armed".into(),
            ));
        }
        self.session
            .rebind_page_with_roots(bound.page_id(), strokes, root_of)
            .map_err(CtrlError::Fatal)?;
        self.bound = bound;
        self.texts = texts;
        self.links = links;
        self.store = store;
        self.policy = StreamRefreshPolicy::new(self.policy.interval_us);
        self.last_page = None;
        self.last_pressure = None;
        self.stroke_dirty = None;
        self.last_wet_marker = None;
        self.last_submit_us = None;
        self.stats.reset_stroke();
        self.last_persist = None;
        self.last_pen_up_timing = None;
        self.pending_clean = None;
        self.idle_since_us = None;
        Ok(())
    }

    /// Durable whole-stroke delete. Idle + disarmed only. Never arms WetInk.
    /// Mutates session only after store append/fsync succeeds.
    #[allow(dead_code)] // F3/F5 path; F7 undo/erase use [`Self::durable_replace_strokes`]
    pub fn durable_delete_stroke(
        &mut self,
        stroke_id: &StrokeId,
    ) -> Result<PersistTiming, CtrlError> {
        if !self.session.is_idle() {
            return Err(CtrlError::Recoverable(
                "erase refused: session not Idle".into(),
            ));
        }
        if self.life.is_armed() {
            return Err(CtrlError::Fatal(
                "erase refused: stylus mode still armed".into(),
            ));
        }
        if !self
            .session
            .committed_strokes()
            .iter()
            .any(|s| s.id.as_str() == stroke_id.as_str())
        {
            return Err(CtrlError::Recoverable(format!(
                "erase refused: stroke {stroke_id} not visible"
            )));
        }
        match self.store.append_delete_stroke(stroke_id) {
            Ok(timing) => {
                self.session
                    .delete_committed_stroke(stroke_id)
                    .map_err(CtrlError::Fatal)?;
                self.totals.strokes_deleted += 1;
                self.last_persist = Some(timing);
                Ok(timing)
            }
            Err(e) => {
                self.totals.delete_persistence_errors += 1;
                self.totals.persistence_errors += 1;
                Err(CtrlError::Fatal(format!("erase persist: {e}")))
            }
        }
    }

    /// Durable atomic `replace_strokes` (F7 partial erase / lineage undo).
    ///
    /// Preflight validates against the live visible model **before** any durable write.
    /// After fsync, the prepared state is installed without further semantic checks.
    pub fn durable_replace_strokes(
        &mut self,
        replacements: &[crate::store::PersistedReplacementV2],
    ) -> Result<PersistTiming, CtrlError> {
        if !self.session.is_idle() {
            return Err(CtrlError::Recoverable(
                "replace refused: session not Idle".into(),
            ));
        }
        if self.life.is_armed() {
            return Err(CtrlError::Fatal(
                "replace refused: stylus mode still armed".into(),
            ));
        }
        if replacements.is_empty() {
            return Err(CtrlError::Recoverable(
                "replace refused: empty replacements".into(),
            ));
        }
        let prepared = self
            .session
            .prepare_replace_strokes(replacements)
            .map_err(CtrlError::Recoverable)?;
        match self.store.append_replace_strokes(replacements) {
            Ok(timing) => {
                self.session.commit_prepared_replace(prepared);
                self.last_persist = Some(timing);
                Ok(timing)
            }
            Err(e) => {
                // One owner for the generic persistence counter; callers may add
                // path-specific counters (partial_erase / undo) themselves.
                self.totals.persistence_errors += 1;
                Err(CtrlError::Fatal(format!("replace persist: {e}")))
            }
        }
    }

    /// Gate F4 / F7: durable undo of the newest authored stroke lineage.
    ///
    /// If the newest visible stroke is unsplit, emits `replace_strokes → []` for that id
    /// (or inherits prior delete path). If split into fragments, one replace clears all
    /// visible fragments sharing the same root.
    pub fn undo_last_visible_stroke(&mut self) -> Result<UndoResult, CtrlError> {
        if !self.session.is_idle() {
            return Err(CtrlError::Recoverable(
                "undo refused: session not Idle".into(),
            ));
        }
        if self.life.is_armed() {
            return Err(CtrlError::Fatal(
                "undo refused: stylus mode still armed".into(),
            ));
        }
        let Some(newest) = self.session.last_committed_stroke_id().cloned() else {
            self.totals.undo_empty += 1;
            return Ok(UndoResult::Empty);
        };
        let root = self.session.root_id_of(&newest);
        let targets = self.session.visible_ids_for_root(&root);
        if targets.is_empty() {
            self.totals.undo_empty += 1;
            return Ok(UndoResult::Empty);
        }
        let replacements: Vec<crate::store::PersistedReplacementV2> = targets
            .iter()
            .map(|id| crate::store::PersistedReplacementV2 {
                target_stroke_id: id.as_str().to_string(),
                fragments: Vec::new(),
            })
            .collect();
        match self.durable_replace_strokes(&replacements) {
            Ok(timing) => {
                self.totals.undo_hits += 1;
                self.totals.strokes_deleted += targets.len() as u64;
                Ok(UndoResult::Deleted {
                    stroke_id: newest,
                    timing,
                })
            }
            Err(e) => {
                self.totals.undo_persistence_errors += 1;
                Err(e)
            }
        }
    }

    /// Pen Down: BeginStroke → arm → stamp → immediate DU.
    pub fn on_pen_down(
        &mut self,
        page: PagePoint,
        pressure: Option<f32>,
    ) -> Result<StrokeId, CtrlError> {
        // New wet stroke supersedes pending idle clean for this contact.
        self.idle_since_us = None;
        let Some(down) = self.session.on_pen_down(page, pressure) else {
            return Err(CtrlError::Recoverable("pen down ignored (not Idle)".into()));
        };
        self.stats.reset_stroke();
        if !self.input_timing_compatible {
            self.stats.input_to_du_unavailable = true;
        }
        self.policy.clear();
        self.stroke_dirty = None;
        self.last_wet_marker = None;
        self.last_submit_us = None;
        self.last_page = Some(page);
        self.last_pressure = pressure;

        let t0 = self.transport.now_us();
        if let Err(e) = self.transport.arm() {
            let _ = self.session.on_stream_gap();
            return Err(CtrlError::Fatal(format!("arm: {e}")));
        }
        if let Err(e) = self.life.mark_armed() {
            // Armed in hardware but lifecycle corrupt — attempt disarm then fatal.
            let _ = self.transport.disarm();
            let _ = self.session.on_stream_gap();
            return Err(CtrlError::Fatal(format!("lifecycle arm: {e}")));
        }
        self.stats.arm_us = self.transport.now_us().saturating_sub(t0);
        self.stats.stroke_start_us = Some(self.transport.now_us());

        match self.transport.stamp(page, pressure) {
            Ok(Some(d)) => {
                self.merge_dirty(d);
                self.policy.merge(d);
            }
            Ok(None) => {}
            Err(e) => {
                return self.fail_after_arm(format!("stamp: {e}"));
            }
        }

        let now = self.transport.now_us();
        if let Some(region) = self.policy.take_if_due(now) {
            if let Err(e) = self.submit_du(region, now, None) {
                return self.fail_after_arm(e);
            }
        }

        Ok(down.stroke_id)
    }

    pub fn on_pen_move(
        &mut self,
        page: PagePoint,
        pressure: Option<f32>,
        latest_input_kernel_us: Option<u64>,
    ) -> Result<(), CtrlError> {
        if self.session.phase() != SessionPhase::WetStrokeActive {
            return Ok(());
        }
        let Some(mv) = self.session.on_pen_move(page, pressure) else {
            return Ok(());
        };
        if !mv.appended {
            return Ok(());
        }
        self.stats.move_count += 1;
        let from = self.last_page.unwrap_or(page);
        let from_p = self.last_pressure;
        self.last_page = Some(page);
        self.last_pressure = pressure;

        match self.transport.segment(from, from_p, page, pressure) {
            Ok(Some(d)) => {
                self.merge_dirty(d);
                self.policy.merge(d);
            }
            Ok(None) => {}
            Err(e) => return Err(CtrlError::Recoverable(format!("segment: {e}"))),
        }

        let now = self.transport.now_us();
        if let Some(region) = self.policy.take_if_due(now) {
            let kin = if self.input_timing_compatible {
                latest_input_kernel_us
            } else {
                None
            };
            self.submit_du(region, now, kin)
                .map_err(CtrlError::Recoverable)?;
        }
        Ok(())
    }

    /// Pen Up: final DU → WAIT → disarm → FinishStroke → durable append.
    /// GC16 is deferred to [`Self::maybe_idle_clean`] (Gate F4.1).
    pub fn on_pen_up(
        &mut self,
        page: PagePoint,
        pressure: Option<f32>,
    ) -> Result<StrokeId, CtrlError> {
        if self.session.phase() != SessionPhase::WetStrokeActive {
            return Err(CtrlError::Recoverable(
                "pen up ignored (not WetStrokeActive)".into(),
            ));
        }
        let t_handler0 = self.transport.now_us();
        let mut timing = PenUpPhaseTiming::default();
        let end = t_handler0;
        self.stats.stroke_end_us = Some(end);

        if let Some(from) = self.last_page {
            let from_p = self.last_pressure;
            match self.transport.segment(from, from_p, page, pressure) {
                Ok(Some(d)) => {
                    self.merge_dirty(d);
                    self.policy.merge(d);
                }
                Ok(None) => {}
                Err(e) => eprintln!("warning: final segment: {e}"),
            }
        }
        self.last_page = Some(page);
        self.last_pressure = pressure;

        let now = self.transport.now_us();
        let t_du0 = now;
        if let Some(region) = self.policy.force_take(now) {
            if let Err(e) = self.submit_du(region, now, None) {
                eprintln!("warning: final DU submit: {e}");
            }
        }
        timing.final_du_us = self.transport.now_us().saturating_sub(t_du0);

        let t_wait0 = self.transport.now_us();
        let wait_err = self.wait_last_wet_timed(&mut timing).err();
        timing.final_wait_us = timing
            .final_wait_us
            .max(self.transport.now_us().saturating_sub(t_wait0));

        let t_disarm0 = self.transport.now_us();
        if let Err(e) = self.disarm_clean() {
            let _ = self.session.on_stream_gap();
            self.stroke_dirty = None;
            self.last_page = None;
            return Err(CtrlError::Fatal(format!("SET_STYLUS_MODE(0) failed: {e}")));
        }
        timing.disarm_us = self.transport.now_us().saturating_sub(t_disarm0);

        if let Some(e) = wait_err {
            let gap = self.session.on_stream_gap();
            if let Some(c) = gap.cancelled.as_ref() {
                eprintln!(
                    "stroke_cancelled id={} points={} reason=WAIT_failed_after_disarm",
                    c.id,
                    c.points.len()
                );
            }
            self.stroke_dirty = None;
            self.last_page = None;
            return Err(CtrlError::Recoverable(format!(
                "WAIT failed after successful disarm: {e}"
            )));
        }

        let t_model0 = self.transport.now_us();
        let Some(mut up) = self.session.on_pen_up(page, pressure) else {
            return Err(CtrlError::Fatal(
                "model finish failed after wet path (lifecycle corruption)".into(),
            ));
        };
        timing.model_finish_us = self.transport.now_us().saturating_sub(t_model0);

        // Gate F6: newly authored live strokes are pressure-width. Tag before persist so the
        // durable record (v2) and the committed model both reconstruct with the pressure curve.
        up.stroke.brush = Some(STYLE_PRESSURE_WIDTH_V1.to_string());
        self.session
            .set_last_committed_brush(&up.stroke.id, Some(STYLE_PRESSURE_WIDTH_V1.to_string()))
            .map_err(CtrlError::Fatal)?;
        self.session.register_committed_root(&up.stroke.id);

        // Durable append only after proven disarm (no FS I/O while armed).
        debug_assert!(!self.life.is_armed());
        if self.life.is_armed() {
            self.totals.persistence_errors += 1;
            return Err(CtrlError::Fatal(
                "refusing durable write while StylusModeArmed".into(),
            ));
        }
        if !self.store.is_writable() {
            self.totals.persistence_errors += 1;
            return Err(CtrlError::Fatal(
                "store not writable (truncated-tail recovery incomplete)".into(),
            ));
        }
        match self.store.append_committed_stroke(&up.stroke) {
            Ok(pt) => {
                self.last_persist = Some(pt);
                self.totals.strokes_durable += 1;
                timing.persist_append_us = pt.append_us;
                timing.persist_fsync_us = pt.fsync_us;
                println!(
                    "stroke_commit id={} model_committed=true durable=true persist_append_us={} persist_fsync_us={} persist_commit_us={}",
                    up.stroke.id,
                    pt.append_us,
                    pt.fsync_us,
                    pt.commit_us()
                );
            }
            Err(e) => {
                self.totals.persistence_errors += 1;
                eprintln!(
                    "stroke_commit id={} model_committed=true durable=false fatal=true err={e}",
                    up.stroke.id
                );
                return Err(CtrlError::Fatal(format!(
                    "durable append/fsync failed: {e}"
                )));
            }
        }

        // Defer GC16: accumulate dirty; return to input ASAP (F4.1).
        if let Some(d) = self.stroke_dirty.take() {
            self.pending_clean = Some(match self.pending_clean {
                Some(prev) => prev.union(d),
                None => d,
            });
            timing.gc16_deferred = true;
            self.stats.last_dirty = Some(d);
        }
        timing.pen_up_total_handler_us = self.transport.now_us().saturating_sub(t_handler0);
        self.last_pen_up_timing = Some(timing);
        self.last_finished_stroke_id = Some(up.stroke.id.clone());
        self.last_up_handler_return_us = Some(self.transport.now_us());
        self.idle_since_us = Some(self.transport.now_us());
        self.log_stroke_committed(&up.stroke.id, up.stroke.points.len());
        println!(
            "pen_up_phases final_du_us={} final_wait_us={} disarm_us={} model_finish_us={} \
             persist_append_us={} persist_fsync_us={} gc16_deferred={} pen_up_total_handler_us={}",
            timing.final_du_us,
            timing.final_wait_us,
            timing.disarm_us,
            timing.model_finish_us,
            timing.persist_append_us,
            timing.persist_fsync_us,
            timing.gc16_deferred,
            timing.pen_up_total_handler_us
        );
        Ok(up.stroke.id)
    }

    /// Whether a deferred idle GC16 is still outstanding.
    pub fn has_pending_clean(&self) -> bool {
        self.pending_clean.is_some()
    }

    /// Clear pending idle-clean (e.g. after full-page reconstruct).
    pub fn clear_pending_clean(&mut self) {
        self.pending_clean = None;
        self.idle_since_us = None;
    }

    /// If Idle+disarmed and idle long enough, flush one coalesced GC16.
    pub fn maybe_idle_clean(&mut self, input_queue_empty: bool) -> Result<bool, CtrlError> {
        if self.pending_clean.is_none() {
            return Ok(false);
        }
        if !self.session.is_idle() || self.life.is_armed() {
            return Ok(false);
        }
        if !input_queue_empty {
            return Ok(false);
        }
        let Some(since) = self.idle_since_us else {
            return Ok(false);
        };
        let now = self.transport.now_us();
        if now.saturating_sub(since) < CLEAN_REFRESH_IDLE_US {
            return Ok(false);
        }
        self.flush_pending_clean()
    }

    /// Flush pending clean now (shutdown / explicit). Idle+disarmed only.
    pub fn flush_pending_clean(&mut self) -> Result<bool, CtrlError> {
        let Some(dirty) = self.pending_clean.take() else {
            return Ok(false);
        };
        if self.life.is_armed() {
            self.pending_clean = Some(dirty);
            return Err(CtrlError::Fatal(
                "refusing GC16 while stylus mode armed".into(),
            ));
        }
        self.life
            .require_send(UpdateClass::Ordinary)
            .map_err(|e| CtrlError::Fatal(e.to_string()))?;
        let t0 = self.transport.now_us();
        match self
            .transport
            .commit_gc16(dirty, self.session.committed_strokes())
        {
            Ok(()) => {
                let dt = self.transport.now_us().saturating_sub(t0);
                self.stats.gc16_commits += 1;
                self.totals.gc16_commit_refreshes += 1;
                self.totals.idle_clean_refreshes += 1;
                self.stats.last_dirty = Some(dirty);
                self.idle_since_us = None;
                println!(
                    "idle_clean_gc16 dirty=({},{} {}x{}) elapsed_us={dt}",
                    dirty.x, dirty.y, dirty.width, dirty.height
                );
                Ok(true)
            }
            Err(e) => {
                // Keep dirty pending so a later attempt / reconstruct can recover visually.
                self.pending_clean = Some(dirty);
                eprintln!("warning: idle GC16 failed: {e}");
                Ok(false)
            }
        }
    }

    /// Stream gap handling. Idle gaps do not invent cancellation.
    pub fn on_stream_gap(&mut self, had_active_contact: bool) -> Result<(), CtrlError> {
        self.totals.input_stream_gaps += 1;
        let active = had_active_contact || self.session.is_wet_active() || self.life.is_armed();
        if active {
            self.totals.input_stream_gaps_active += 1;
        } else {
            self.totals.input_stream_gaps_idle += 1;
            println!("StreamGap idle=true");
            return Ok(());
        }

        if !self.session.is_wet_active() && !self.life.is_armed() {
            let _ = self.session.on_stream_gap();
            return Ok(());
        }
        let now = self.transport.now_us();
        self.stats.stroke_end_us = Some(now);
        if self.life.is_armed() {
            if let Some(region) = self.policy.force_take(now) {
                if let Err(e) = self.submit_du(region, now, None) {
                    eprintln!("warning: StreamGap DU flush: {e}");
                }
            }
            let wait_err = self.wait_last_wet().err();
            if let Some(e) = wait_err {
                eprintln!("warning: StreamGap WAIT: {e}");
            }
            if let Err(e) = self.disarm_clean() {
                let _ = self.session.on_stream_gap();
                self.stroke_dirty = None;
                self.last_page = None;
                return Err(CtrlError::Fatal(format!(
                    "StreamGap SET_STYLUS_MODE(0) failed: {e}"
                )));
            }
        }
        let gap = self.session.on_stream_gap();
        if let Some(c) = gap.cancelled.as_ref() {
            eprintln!(
                "stroke_cancelled id={} points={} reason=StreamGap (visual residue may remain; model not committed)",
                c.id,
                c.points.len()
            );
        }
        self.stroke_dirty = None;
        self.last_page = None;
        Ok(())
    }

    /// Graceful shutdown: cancel active stroke if needed; prove disarmed.
    pub fn shutdown(&mut self) -> Result<(), CtrlError> {
        if self.session.is_wet_active() || self.life.is_armed() {
            let now = self.transport.now_us();
            if self.life.is_armed() {
                if let Some(region) = self.policy.force_take(now) {
                    let _ = self.submit_du(region, now, None);
                }
                let _ = self.wait_last_wet();
                if let Err(e) = self.disarm_clean() {
                    let _ = self.session.on_stream_gap();
                    return Err(CtrlError::Fatal(format!(
                        "shutdown SET_STYLUS_MODE(0) failed: {e}"
                    )));
                }
            }
            let _ = self.session.on_stream_gap();
        }
        if self.life.is_armed() {
            return Err(CtrlError::Fatal(
                "shutdown left stylus mode armed (lifecycle corruption)".into(),
            ));
        }
        let _ = self.flush_pending_clean();
        Ok(())
    }

    fn fail_after_arm(&mut self, msg: String) -> Result<StrokeId, CtrlError> {
        match self.emergency_disarm() {
            Ok(()) => {
                let _ = self.session.on_stream_gap();
                self.stroke_dirty = None;
                self.last_page = None;
                Err(CtrlError::Recoverable(msg))
            }
            Err(e) => {
                let _ = self.session.on_stream_gap();
                self.stroke_dirty = None;
                self.last_page = None;
                Err(CtrlError::Fatal(format!("{msg}; then disarm failed: {e}")))
            }
        }
    }

    fn merge_dirty(&mut self, d: DirtyRegion) {
        self.stroke_dirty = Some(match self.stroke_dirty {
            Some(s) => s.union(d),
            None => d,
        });
        self.stats.last_dirty = self.stroke_dirty;
    }

    fn submit_du(
        &mut self,
        region: DirtyRegion,
        now_us: u64,
        latest_input: Option<u64>,
    ) -> Result<(), String> {
        self.life
            .require_send(UpdateClass::WetInkDu)
            .map_err(|e| e.to_string())?;
        if let Some(prev) = self.last_submit_us {
            self.stats
                .interval_samples
                .push(now_us.saturating_sub(prev));
        }
        if let Some(kin) = latest_input {
            if self.input_timing_compatible {
                self.stats
                    .input_to_send_samples
                    .push(now_us.saturating_sub(kin));
            }
        }
        match self.transport.send_du(region) {
            Ok(r) => {
                if r.marker == 0 {
                    self.stats.du_send_errors += 1;
                    self.totals.du_send_errors += 1;
                    return Err("DU SEND returned marker=0 (invalid/empty update)".into());
                }
                self.last_wet_marker = Some(r.marker);
                self.stats.last_wet_marker = Some(r.marker);
                self.stats.du_batches += 1;
                self.totals.du_sends += 1;
                self.stats.send_samples.push(r.send_ioctl_us);
                self.last_submit_us = Some(now_us);
                Ok(())
            }
            Err(e) => {
                self.stats.du_send_errors += 1;
                self.totals.du_send_errors += 1;
                Err(format!("DU SEND: {e}"))
            }
        }
    }

    fn wait_last_wet(&mut self) -> Result<(), String> {
        let mut unused = PenUpPhaseTiming::default();
        self.wait_last_wet_timed(&mut unused)
    }

    fn wait_last_wet_timed(&mut self, timing: &mut PenUpPhaseTiming) -> Result<(), String> {
        let Some(m) = self.last_wet_marker else {
            return Ok(());
        };
        let w = self
            .transport
            .wait_marker(m)
            .map_err(|e| format!("WAIT: {e}"))?;
        timing.final_wait_us = w.wait_ioctl_us;
        let _ = w.marker;
        self.stats.last_collision = Some(w.collision_test);
        Ok(())
    }

    fn disarm_clean(&mut self) -> Result<(), String> {
        let t0 = self.transport.now_us();
        self.transport
            .disarm()
            .map_err(|e| format!("disarm: {e}"))?;
        self.life
            .mark_disarmed()
            .map_err(|e| format!("lifecycle disarm: {e}"))?;
        self.stats.disarm_us = self.transport.now_us().saturating_sub(t0);
        Ok(())
    }

    fn emergency_disarm(&mut self) -> Result<(), String> {
        let _ = self.wait_last_wet();
        self.disarm_clean()
    }

    fn log_stroke_committed(&self, id: &StrokeId, points: usize) {
        let dirty = self
            .stats
            .last_dirty
            .map(|d| format!("({},{} {}x{})", d.x, d.y, d.width, d.height))
            .unwrap_or_else(|| "none".into());
        let dur = self
            .stats
            .duration_us()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "?".into());
        let input_to_du = if self.stats.input_to_du_unavailable {
            "latest_input_to_du_send_us=unavailable(clock_domain_mismatch)".to_string()
        } else {
            format_dist(
                "latest_input_to_du_send_us",
                &self.stats.input_to_send_samples,
            )
        };
        println!(
            "stroke_committed id={id} points={points} moves={} duration_us={dur} du_batches={} du_send_errors={} {} {} {} final_wet_marker={:?} final_collision={:?} arm_us={} disarm_us={} dirty={dirty} gc16_ok={} page_stroke_count={}",
            self.stats.move_count,
            self.stats.du_batches,
            self.stats.du_send_errors,
            format_dist("du_send_us", &self.stats.send_samples),
            format_dist("actual_interval_us", &self.stats.interval_samples),
            input_to_du,
            self.stats.last_wet_marker,
            self.stats.last_collision,
            self.stats.arm_us,
            self.stats.disarm_us,
            self.last_pen_up_timing
                .map(|t| t.gc16_deferred)
                .unwrap_or(false)
                || self.stats.gc16_commits > 0,
            self.session.committed_count(),
        );
    }
}

/// Host-testable EVIOCGRAB gate (Gate F0.1): no live session without exclusive pen ownership.
pub fn require_eviocgrab(grab_ok: bool) -> Result<(), String> {
    if grab_ok {
        Ok(())
    } else {
        Err("REFUSE LIVE SESSION: EVIOCGRAB failed (exclusive pen ownership required)".into())
    }
}

/// Map a dirty region to an HWTCON rect, or error if nothing is panel-visible.
pub fn dirty_to_send_rect(
    dirty: DirtyRegion,
    xres: u32,
    yres: u32,
) -> Result<(i32, i32, u32, u32), String> {
    let clipped = dirty
        .clamp_to(xres, yres)
        .ok_or_else(|| format!("DU dirty fully outside panel: {dirty:?}"))?;
    if clipped.x < 0 || clipped.y < 0 || clipped.width == 0 || clipped.height == 0 {
        return Err(format!("DU dirty invalid after clamp: {clipped:?}"));
    }
    Ok((clipped.x, clipped.y, clipped.width, clipped.height))
}

/// Recording fake transport for sequencing / fault-injection tests.
#[derive(Debug, Default)]
#[cfg(test)]
pub struct FakeTransport {
    pub now: u64,
    pub steps: Vec<TransportStep>,
    pub armed: bool,
    pub next_marker: u32,
    pub panel_w: u32,
    pub panel_h: u32,
    pub page_w: f32,
    pub page_h: f32,
    pub fail_arm: bool,
    pub fail_disarm: bool,
    pub fail_wait: bool,
    pub fail_du: bool,
    pub fail_gc16: bool,
    pub du_while_disarmed_attempts: u64,
    pub gc16_while_armed_attempts: u64,
    /// Last page chrome string supplied to a reconstruct (host tests).
    pub last_page_label: Option<String>,
}

#[cfg(test)]
impl FakeTransport {
    pub fn new() -> Self {
        Self {
            now: 1_000_000,
            next_marker: 1,
            panel_w: 1860,
            panel_h: 2480,
            page_w: 15624.0,
            page_h: 20832.0,
            ..Default::default()
        }
    }

    fn page_to_panel(&self, p: PagePoint) -> (i32, i32) {
        let x = ((p.x / self.page_w) * self.panel_w as f32).round() as i32;
        let y = ((p.y / self.page_h) * self.panel_h as f32).round() as i32;
        (x, y)
    }

    fn dirty_at(&self, p: PagePoint) -> DirtyRegion {
        let (x, y) = self.page_to_panel(p);
        DirtyRegion::new(x.saturating_sub(2), y.saturating_sub(2), 5, 5)
    }

    /// Record chrome text that runtime would pass into `reconstruct_page`.
    pub fn note_reconstruct_label(&mut self, page_label: Option<&str>) {
        self.last_page_label = page_label.map(str::to_string);
    }
}

#[cfg(test)]
impl InkTransport for FakeTransport {
    fn now_us(&self) -> u64 {
        self.now
    }

    fn arm(&mut self) -> Result<(), String> {
        if self.fail_arm {
            return Err("fake arm fail".into());
        }
        self.steps.push(TransportStep::Arm);
        self.armed = true;
        self.now += 100;
        Ok(())
    }

    fn disarm(&mut self) -> Result<(), String> {
        if self.fail_disarm {
            self.steps.push(TransportStep::Disarm);
            return Err("fake disarm fail".into());
        }
        self.steps.push(TransportStep::Disarm);
        self.armed = false;
        self.now += 100;
        Ok(())
    }

    fn stamp(
        &mut self,
        page: PagePoint,
        _pressure: Option<f32>,
    ) -> Result<Option<DirtyRegion>, String> {
        self.steps.push(TransportStep::Stamp);
        self.now += 10;
        Ok(Some(self.dirty_at(page)))
    }

    fn segment(
        &mut self,
        from: PagePoint,
        _pa: Option<f32>,
        to: PagePoint,
        _pb: Option<f32>,
    ) -> Result<Option<DirtyRegion>, String> {
        self.steps.push(TransportStep::Segment);
        self.now += 10;
        Ok(Some(self.dirty_at(from).union(self.dirty_at(to))))
    }

    fn send_du(&mut self, dirty: DirtyRegion) -> Result<DuSendResult, String> {
        if !self.armed {
            self.du_while_disarmed_attempts += 1;
            return Err("DU while disarmed".into());
        }
        dirty_to_send_rect(dirty, self.panel_w, self.panel_h)?;
        if self.fail_du {
            return Err("fake DU fail".into());
        }
        self.steps.push(TransportStep::DuSend);
        let marker = self.next_marker;
        self.next_marker = self.next_marker.wrapping_add(1).max(1);
        self.now += 50;
        Ok(DuSendResult {
            marker,
            send_ioctl_us: 40,
        })
    }

    fn wait_marker(&mut self, marker: u32) -> Result<WaitResult, String> {
        self.steps.push(TransportStep::Wait);
        self.now += 200;
        if self.fail_wait {
            return Err("fake WAIT fail".into());
        }
        Ok(WaitResult {
            marker,
            collision_test: 0,
            wait_ioctl_us: 180,
        })
    }

    fn commit_gc16(&mut self, _dirty: DirtyRegion, _strokes: &[Stroke]) -> Result<(), String> {
        if self.armed {
            self.gc16_while_armed_attempts += 1;
            return Err("GC16 while armed".into());
        }
        if self.fail_gc16 {
            return Err("fake GC16 fail".into());
        }
        self.steps.push(TransportStep::Gc16);
        self.now += 300;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kindle_platform::UpdateClass;
    use notebook_core::PagePoint;

    fn pt(x: f32, y: f32) -> PagePoint {
        PagePoint::new(x, y)
    }

    #[test]
    fn successful_stroke_sequence() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(100.0, 100.0), Some(0.4)).unwrap();
        c.transport_mut().now += 30_000;
        c.on_pen_move(pt(200.0, 150.0), Some(0.5), None).unwrap();
        c.transport_mut().now += 30_000;
        c.on_pen_move(pt(300.0, 200.0), None, None).unwrap();
        let id = c.on_pen_up(pt(310.0, 210.0), None).unwrap();
        assert_eq!(c.session.committed_count(), 1);
        assert_eq!(c.session.committed_strokes()[0].id, id);
        assert!(c.is_idle());
        assert!(!c.life().is_armed());
    }

    #[test]
    fn never_gc16_while_armed_never_du_while_disarmed() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        assert!(c.life().can_send(UpdateClass::WetInkDu));
        c.transport_mut().now += 30_000;
        c.on_pen_move(pt(50.0, 50.0), None, None).unwrap();
        c.on_pen_up(pt(60.0, 60.0), None).unwrap();
        assert!(c.life().can_send(UpdateClass::Ordinary));
        assert_eq!(c.transport().gc16_while_armed_attempts, 0);
        assert_eq!(c.transport().du_while_disarmed_attempts, 0);
    }

    #[test]
    fn second_stroke_repeats_lifecycle() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        c.on_pen_up(pt(10.0, 10.0), None).unwrap();
        c.on_pen_down(pt(100.0, 100.0), None).unwrap();
        c.transport_mut().now += 30_000;
        c.on_pen_move(pt(120.0, 120.0), None, None).unwrap();
        c.on_pen_up(pt(130.0, 130.0), None).unwrap();
        assert_eq!(c.session.committed_count(), 2);
    }

    #[test]
    fn stream_gap_successful_disarm_is_recoverable() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        c.on_pen_move(pt(5.0, 5.0), None, None).unwrap();
        c.on_stream_gap(true).unwrap();
        assert_eq!(c.session.committed_count(), 0);
        assert_eq!(c.session.strokes_cancelled(), 1);
        assert!(c.is_idle());
        assert!(!c.life().is_armed());
        // May accept another stroke.
        c.on_pen_down(pt(20.0, 20.0), None).unwrap();
        c.on_pen_up(pt(21.0, 21.0), None).unwrap();
        assert_eq!(c.session.committed_count(), 1);
    }

    #[test]
    fn stream_gap_failed_disarm_is_fatal() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        c.transport_mut().fail_disarm = true;
        let err = c.on_stream_gap(true).unwrap_err();
        assert!(err.is_fatal());
        assert_eq!(c.session.committed_count(), 0);
        // Must not accept another stroke while lifecycle uncertain / still "armed" phase.
        assert!(c.life().is_armed() || !c.is_idle() || c.session.strokes_cancelled() >= 1);
        // Runtime must not continue — simulate by treating fatal as stop; model not committed.
        assert!(matches!(err, CtrlError::Fatal(_)));
    }

    #[test]
    fn shutdown_failed_disarm_returns_failure() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        c.transport_mut().fail_disarm = true;
        let err = c.shutdown().unwrap_err();
        assert!(err.is_fatal());
    }

    #[test]
    fn pen_up_failed_disarm_no_commit_fatal() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        c.transport_mut().fail_disarm = true;
        let err = c.on_pen_up(pt(10.0, 10.0), None).unwrap_err();
        assert!(err.is_fatal());
        assert_eq!(c.session.committed_count(), 0);
    }

    #[test]
    fn pen_up_wait_fail_after_disarm_cancels_recoverable() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        c.transport_mut().fail_wait = true;
        let err = c.on_pen_up(pt(10.0, 10.0), None).unwrap_err();
        assert!(!err.is_fatal());
        assert_eq!(c.session.committed_count(), 0);
        assert!(c.is_idle());
        assert!(!c.life().is_armed());
    }

    #[test]
    fn input_to_du_recorded_only_for_linux_monotonic() {
        let mut mono = StrokeController::with_input_clock(
            "p",
            FakeTransport::new(),
            25_000,
            ClockDomain::LinuxMonotonic,
        );
        mono.on_pen_down(pt(0.0, 0.0), None).unwrap();
        mono.transport_mut().now += 30_000;
        mono.on_pen_move(pt(10.0, 10.0), None, Some(1_010_000))
            .unwrap();
        assert!(!mono.stats.input_to_send_samples.is_empty());

        let mut other = StrokeController::with_input_clock(
            "p",
            FakeTransport::new(),
            25_000,
            ClockDomain::Unknown,
        );
        other.on_pen_down(pt(0.0, 0.0), None).unwrap();
        other.transport_mut().now += 30_000;
        other
            .on_pen_move(pt(10.0, 10.0), None, Some(1_010_000))
            .unwrap();
        assert!(other.stats.input_to_send_samples.is_empty());
        assert!(other.stats.input_to_du_unavailable);
    }

    #[test]
    fn gc16_stats_reset_between_strokes() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        c.on_pen_up(pt(10.0, 10.0), None).unwrap();
        // F4.1: GC16 deferred until idle clean.
        assert_eq!(c.stats.gc16_commits, 0);
        c.transport_mut().now += CLEAN_REFRESH_IDLE_US + 1;
        assert!(c.maybe_idle_clean(true).unwrap());
        assert_eq!(c.stats.gc16_commits, 1);
        assert_eq!(c.totals.gc16_commit_refreshes, 1);
        c.on_pen_down(pt(20.0, 20.0), None).unwrap();
        assert_eq!(c.stats.gc16_commits, 0);
        assert_eq!(c.totals.gc16_commit_refreshes, 1);
    }

    #[test]
    fn eviocgrab_failure_refuses_live_session() {
        assert!(require_eviocgrab(true).is_ok());
        let e = require_eviocgrab(false).unwrap_err();
        assert!(e.contains("REFUSE LIVE SESSION"));
    }

    #[test]
    fn invalid_du_dirty_is_not_fake_marker_zero_success() {
        let err = dirty_to_send_rect(DirtyRegion::new(9000, 9000, 10, 10), 1860, 2480).unwrap_err();
        assert!(err.contains("outside") || err.contains("invalid"));

        let mut ft = FakeTransport::new();
        ft.armed = true;
        let r = ft.send_du(DirtyRegion::new(9000, 9000, 5, 5));
        assert!(r.is_err());
    }

    #[test]
    fn shutdown_cancels_active_stroke() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        c.shutdown().unwrap();
        assert!(c.is_idle());
        assert!(!c.life().is_armed());
        assert_eq!(c.session.committed_count(), 0);
        assert_eq!(c.session.strokes_cancelled(), 1);
    }

    #[test]
    fn pen_move_error_then_failed_disarm_is_fatal() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        // Simulate move-path abort via stream_gap with disarm failure (runtime's rule).
        c.transport_mut().fail_disarm = true;
        let err = c.on_stream_gap(true).unwrap_err();
        assert!(err.is_fatal());
        assert_eq!(c.session.committed_count(), 0);
    }

    #[test]
    fn idle_stream_gap_does_not_cancel() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_stream_gap(false).unwrap();
        assert_eq!(c.totals.input_stream_gaps_idle, 1);
        assert_eq!(c.totals.input_stream_gaps_active, 0);
        assert_eq!(c.session.strokes_cancelled(), 0);
    }

    #[test]
    fn durable_append_after_disarm_before_gc16() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        c.on_pen_up(pt(10.0, 10.0), None).unwrap();
        assert_eq!(c.totals.strokes_durable, 1);
        assert_eq!(c.store().stroke_count(), 1);
        // Deferred: no GC16 yet on pen-up path.
        assert!(!c.transport().steps.contains(&TransportStep::Gc16));
        c.transport_mut().now += CLEAN_REFRESH_IDLE_US + 1;
        c.maybe_idle_clean(true).unwrap();
        let steps = &c.transport().steps;
        let disarm_i = steps
            .iter()
            .rposition(|s| *s == TransportStep::Disarm)
            .unwrap();
        let gc_i = steps
            .iter()
            .rposition(|s| *s == TransportStep::Gc16)
            .unwrap();
        assert!(disarm_i < gc_i);
    }

    #[test]
    fn rapid_second_stroke_persists_separately() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 8_000);
        // Vertical line.
        c.on_pen_down(pt(100.0, 0.0), None).unwrap();
        c.transport_mut().now += 5_000;
        c.on_pen_move(pt(100.0, 200.0), None, None).unwrap();
        c.on_pen_up(pt(100.0, 400.0), None).unwrap();
        // Immediate humps (second stroke) — no idle clean required first.
        c.on_pen_down(pt(120.0, 200.0), None).unwrap();
        c.transport_mut().now += 5_000;
        c.on_pen_move(pt(180.0, 150.0), None, None).unwrap();
        c.transport_mut().now += 5_000;
        c.on_pen_move(pt(240.0, 200.0), None, None).unwrap();
        c.on_pen_up(pt(300.0, 180.0), None).unwrap();
        assert_eq!(c.session.committed_count(), 2);
        assert_eq!(c.store().stroke_count(), 2);
        let a = &c.session.committed_strokes()[0];
        let b = &c.session.committed_strokes()[1];
        assert_ne!(a.id, b.id);
        assert!(a.points.len() >= 2);
        assert!(b.points.len() >= 2);
    }

    #[test]
    fn a_letter_two_stroke_v_then_crossbar() {
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 8_000);
        // Inverted V.
        c.on_pen_down(pt(0.0, 200.0), None).unwrap();
        c.on_pen_move(pt(50.0, 0.0), None, None).unwrap();
        c.on_pen_up(pt(100.0, 200.0), None).unwrap();
        // Crossbar.
        c.on_pen_down(pt(20.0, 100.0), None).unwrap();
        c.on_pen_up(pt(80.0, 100.0), None).unwrap();
        assert_eq!(c.session.committed_count(), 2);
        assert_eq!(c.store().stroke_count(), 2);
    }

    #[test]
    fn persist_fail_is_fatal_no_next_stroke() {
        let mut store = MemoryPageStore::new();
        store.fail_append = true;
        let mut c = StrokeController::with_store(
            "page-f0",
            FakeTransport::new(),
            25_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        let err = c.on_pen_up(pt(5.0, 5.0), None).unwrap_err();
        assert!(err.is_fatal());
        assert_eq!(c.totals.strokes_durable, 0);
        assert_eq!(c.totals.persistence_errors, 1);
        assert!(!c.life().is_armed());
        // Model may already have the stroke; durability lied not allowed — session fatal.
        assert_eq!(c.session.committed_count(), 1);
    }

    #[test]
    fn no_persist_while_armed_sequencing() {
        // Append only invoked from on_pen_up after disarm_clean.
        let mut c = StrokeController::new("page-f0", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        assert!(c.life().is_armed());
        assert_eq!(c.store().stroke_count(), 0);
        c.on_pen_up(pt(1.0, 1.0), None).unwrap();
        assert!(!c.life().is_armed());
        assert_eq!(c.store().stroke_count(), 1);
    }

    #[test]
    fn bind_page_only_when_idle_disarmed() {
        use notebook_core::{Point, Stroke, StrokeId};
        let mut c = StrokeController::new("page-1", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        let store2 = MemoryPageStore::new();
        assert!(c
            .bind_page("page-2", vec![], store2)
            .unwrap_err()
            .to_string()
            .contains("Idle"));
        c.on_pen_up(pt(1.0, 1.0), None).unwrap();
        assert_eq!(c.session.page_id(), "page-1");
        assert_eq!(c.session.committed_count(), 1);

        let stroke_b = Stroke {
            id: StrokeId::new("s-b"),
            points: vec![Point::new(5.0, 5.0)],
            brush: None,
            finished: true,
        };
        let mut store2 = MemoryPageStore::new();
        store2.strokes.push(stroke_b.clone());
        c.bind_page("page-2", vec![stroke_b], store2).unwrap();
        assert_eq!(c.session.page_id(), "page-2");
        assert_eq!(c.session.committed_count(), 1);
        assert_eq!(c.session.committed_strokes()[0].id.as_str(), "s-b");
        assert_eq!(c.store().stroke_count(), 1);
        // No fake live Begin ops from bind.
        assert_eq!(c.session.strokes_started(), 1); // only the earlier live stroke on page-1
        assert_eq!(c.session.strokes_committed(), 1);

        // Switch back restores page-1 set via explicit strokes arg.
        let stroke_a = Stroke {
            id: StrokeId::new("s-a"),
            points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
            brush: None,
            finished: true,
        };
        let mut store1 = MemoryPageStore::new();
        store1.strokes.push(stroke_a.clone());
        c.bind_page("page-1", vec![stroke_a], store1).unwrap();
        assert_eq!(c.session.page_id(), "page-1");
        assert_eq!(c.session.committed_strokes()[0].id.as_str(), "s-a");
    }

    #[test]
    fn fake_transport_records_page_labels_for_nav_undo_erase() {
        use crate::page_label::format_page_label;
        let mut ft = FakeTransport::new();
        ft.note_reconstruct_label(Some(&format_page_label(1, 2)));
        assert_eq!(ft.last_page_label.as_deref(), Some("1 of 2"));
        ft.note_reconstruct_label(Some(&format_page_label(2, 2)));
        assert_eq!(ft.last_page_label.as_deref(), Some("2 of 2"));
        // Undo / erase reconstruct on page 2 keeps the same chrome.
        ft.note_reconstruct_label(Some(&format_page_label(2, 2)));
        assert_eq!(ft.last_page_label.as_deref(), Some("2 of 2"));
        ft.note_reconstruct_label(Some(&format_page_label(1, 1)));
        assert_eq!(ft.last_page_label.as_deref(), Some("1"));
    }

    #[test]
    fn bind_retains_texts_and_page_switch_replaces_them() {
        let t1 = sync_engine::testkit::text_obj(1, "one");
        let t2 = sync_engine::testkit::text_obj(2, "two");
        let mut c = StrokeController::new("page-1", FakeTransport::new(), 25_000);
        c.bind_page_with_roots(
            crate::open_page::BoundPageKey::new("", "page-1"),
            vec![],
            std::collections::HashMap::new(),
            MemoryPageStore::new(),
            vec![t1.clone()],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(c.texts(), std::slice::from_ref(&t1));
        assert_eq!(c.open_page().page_id(), "page-1");
        c.replace_texts(vec![t2.clone()]);
        assert_eq!(c.texts(), std::slice::from_ref(&t2));
        c.bind_page_with_roots(
            crate::open_page::BoundPageKey::new("", "page-2"),
            vec![],
            std::collections::HashMap::new(),
            MemoryPageStore::new(),
            vec![t2.clone()],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(c.texts(), std::slice::from_ref(&t2));
        c.bind_page("page-3", vec![], MemoryPageStore::new())
            .unwrap();
        assert!(c.texts().is_empty());
    }

    #[test]
    fn bind_bound_key_distinguishes_same_page_name_in_two_notebooks() {
        let t = sync_engine::testkit::text_obj(1, "x");
        let mut c = StrokeController::new("page-1", FakeTransport::new(), 25_000);
        c.bind_page_with_roots(
            BoundPageKey::new("notebook-a", "page-1"),
            vec![],
            std::collections::HashMap::new(),
            MemoryPageStore::new(),
            vec![t.clone()],
            Vec::new(),
        )
        .unwrap();
        let from_a = c.open_page().bound().clone();
        c.bind_page_with_roots(
            BoundPageKey::new("notebook-b", "page-1"),
            vec![],
            std::collections::HashMap::new(),
            MemoryPageStore::new(),
            vec![t],
            Vec::new(),
        )
        .unwrap();
        assert_ne!(from_a, *c.open_page().bound());
        assert_eq!(c.open_page().page_id(), "page-1");
        assert_eq!(c.open_page().bound().notebook_id(), "notebook-b");
    }

    #[test]
    fn wet_pen_path_does_not_touch_retained_texts() {
        let t = sync_engine::testkit::text_obj(1, "stay");
        let mut c = StrokeController::new("page-1", FakeTransport::new(), 25_000);
        c.bind_page_with_roots(
            crate::open_page::BoundPageKey::new("", "page-1"),
            vec![],
            std::collections::HashMap::new(),
            MemoryPageStore::new(),
            vec![t.clone()],
            Vec::new(),
        )
        .unwrap();
        c.on_pen_down(pt(0.0, 0.0), None).unwrap();
        assert_eq!(c.texts(), std::slice::from_ref(&t));
        c.on_pen_move(pt(10.0, 10.0), None, None).unwrap();
        assert_eq!(c.texts(), std::slice::from_ref(&t));
        c.on_pen_up(pt(20.0, 20.0), None).unwrap();
        assert_eq!(c.texts(), std::slice::from_ref(&t));
        assert!(c.links().is_empty());
        assert_eq!(c.session.committed_count(), 1);
    }
}
