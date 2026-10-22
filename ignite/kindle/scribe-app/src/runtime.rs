//! Kindle multi-notebook runtime (Gate F0–F10).
//!
//! One framebuffer + one EVIOCGRAB for the process lifetime.
//! F2: notebook.meta + BTN_STYLUS edge navigation.
//! F3/F5: whole-stroke eraser (`StylusTool::Eraser` / BTN_TOOL_RUBBER) + edits.jsonl.
//! F4: side+center durable undo of newest visible stroke (reuses F3 delete).
//! F4.1: dedicated InputPump + deferred idle GC16.
//! F5: continuous eraser sweep (durable deletes during gesture; reconstruct on Up).
//! F8/F9: durable page deletion, ordering, and insertion within one notebook root.
//! F10: durable library + framebuffer-native Home above the unchanged page engine.
//! F11: launch into Home; pen-operated Home/modals/keyboard/notebook chrome drive the same
//! durable primitives. Chrome is live framebuffer only (never persisted).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kindle_platform::gate_b;
use kindle_platform::hwtcon_abi::stylus_mode;
use kindle_platform::linux::{
    LinuxEvdevStylus, LinuxEvdevTouch, LinuxFramebuffer, LinuxMonotonicClock, MappedFramebuffer,
    TouchPoll, TouchPump,
};
use kindle_platform::{
    raster_dashed_line, raster_line, raster_stamp, DirtyRegion, FbGeometry, InputPump,
    MonotonicClock, MxcfbRect, SourcePoll, StylusEventKind, StylusSource, StylusTool,
    PREFERRED_STYLUS_PATH,
};
use notebook_core::{
    portrait_wacom_calibration, DeviceCalibration, DevicePoint, DisplayPoint, PagePoint, Point,
    Stroke, ERASER_HIT_RADIUS_CANONICAL,
};

use crate::app_ui::{DownRoute, NotebookUiContext, PenRouter, Redraw, UiCommand, UiState};
use crate::cadence::du_target_hz;
use crate::chrome::{page_chrome_for, render_notebook_chrome, ChromeLayout, PageChrome};
use crate::display_events::{spawn_stdin_reader, DisplayEventCounters, DisplayEventReceiver};
use crate::eraser::EraserGestureState;
use crate::folders::{FolderInfo, FolderStore};
use crate::fullscreen::{
    compose_notebook_layers, recompose_dirty_notebook_layers,
    recompose_dirty_notebook_layers_translated,
};
use crate::home::{home_cards, render_home_view, AppView, HomeCard, HomeState};
use crate::ink::parse_ink_request;
use crate::ink::InkRequest;
use crate::keyboard::render_title_editor;
use crate::lasso::{
    move_dirty_bounds, move_replacements, LassoSelection, LassoState, MoveCommit, TouchOverlayDirty,
};
use crate::library::{
    consume_dev_library_op, create_notebook_durable, delete_notebook_durable, open_active_notebook,
    open_or_create_library_shell, peek_dev_library_op, rename_notebook_durable,
    switch_notebook_durable, CreateNotebookHooks, DeleteNotebookHooks, DevLibraryOp,
    LibraryRepository, NotebookQuarantineOutcome, RenameNotebookHooks, SwitchNotebookCommit,
    SwitchNotebookHooks, DEV_CREATE_NOTEBOOK_TRIGGER, DEV_DELETE_NOTEBOOK_TRIGGER,
    DEV_HOME_TRIGGER, DEV_LIBRARY_NEXT_TRIGGER, DEV_LIBRARY_OPEN_TRIGGER, DEV_LIBRARY_PREV_TRIGGER,
};
use crate::link_picker::{LinkPickerState, PendingLink};
use crate::modal::{dialog_spec, render_dialog, DialogSpec, Modal, TitlePurpose};
use crate::nav::{dispatch_side_button, NavDownAction, NavGestureState, SideButtonDispatch};
use crate::notebook::{
    allocate_page_id, commit_new_page_to_manifest, consume_dev_notebook_op,
    create_page_files_from_template, delete_page_durable, insert_page_after_durable,
    load_page_for_switch, move_page_durable, page_index, peek_dev_notebook_op, persist_active_page,
    DeletePageHooks, DevNotebookOp, InsertPageHooks, MovePageDirection, MovePageHooks,
    NotebookOpen, PageSwitchDirection, PersistedNotebookManifestV1, QuarantineOutcome,
    DEV_DELETE_CURRENT_PAGE_TRIGGER, DEV_INSERT_PAGE_AFTER_TRIGGER, DEV_MOVE_PAGE_LEFT_TRIGGER,
    DEV_MOVE_PAGE_RIGHT_TRIGGER, NOTEBOOK_MANIFEST_VERSION,
};
use crate::open_page::BoundPageKey;
use crate::orch::{
    dirty_to_send_rect, input_kernel_for_du_timing, require_eviocgrab, CtrlError, DuSendResult,
    InkTransport, StrokeController, UndoResult, WaitResult,
};
use crate::prefs::{self, SideButtonAction};
use crate::pressure::PressureStrokeSummary;
use crate::profile::RenderProfile;
use crate::store::{page_dir, resolve_data_dir, PageStore, RuntimeStore, DEFAULT_PAGE_ID};
use crate::text_paint::TextOverlay;
use crate::touch::{
    CoalesceDecision, FingerAction, MovePreviewCoalesce, TouchArbiter, TouchMapping,
};
use crate::ui_gfx::{paper_background, Rect, UiPoint, BLACK};

type KindleCtrl = StrokeController<KindleTransport, RuntimeStore>;

pub struct KindleTransport {
    fb: MappedFramebuffer,
    geometry: FbGeometry,
    profile: RenderProfile,
    wall: LinuxMonotonicClock,
    armed: bool,
    /// Notebook title painted in live chrome (display only).
    chrome_title: String,
    chrome_layout: ChromeLayout,
    /// Live ink painted over a chrome band; repaint once idle + disarmed.
    chrome_damaged: bool,
    /// F11.3: whether notebook composition paints the chrome overlay (from `UiState`).
    chrome_visible: bool,
    /// Whether the chrome overlay is currently on the panel (last notebook composition).
    chrome_on_panel: bool,
    /// Prepared current-page text coverage (rebuilt on page/text change, not on the pen path).
    text_overlay: TextOverlay,
    /// Display-only link icons for the bound page (derived from retained `SyncPageLink`s).
    page_links: Vec<sync_protocol::SyncPageLink>,
    /// Last composed durable background (idle GC16 restores this in the dirty rect).
    page_background: Vec<u8>,
    /// Dash-cycle position for the in-progress finger-lasso polyline.
    lasso_dash_phase: u32,
    /// True after the first lasso segment of the current gesture (skip join pixels).
    lasso_dash_started: bool,
}

impl KindleTransport {
    pub fn new(fb: MappedFramebuffer, profile: RenderProfile) -> Self {
        let geometry = fb.geometry;
        Self {
            fb,
            geometry,
            profile,
            wall: LinuxMonotonicClock,
            armed: false,
            chrome_title: String::new(),
            chrome_layout: ChromeLayout::new(&geometry),
            chrome_damaged: false,
            chrome_visible: false,
            chrome_on_panel: false,
            text_overlay: TextOverlay::empty(),
            page_links: Vec::new(),
            page_background: Vec::new(),
            lasso_dash_phase: 0,
            lasso_dash_started: false,
        }
    }

    /// F11.3: the single switch deciding whether notebook reconstructs overlay chrome.
    fn set_chrome_visible(&mut self, visible: bool) {
        self.chrome_visible = visible;
    }

    fn chrome_on_panel(&self) -> bool {
        self.chrome_on_panel
    }

    fn set_chrome_title(&mut self, title: &str) {
        self.chrome_title = title.to_string();
    }

    /// Rebuild the display-only overlay from authoritative texts (idle/open/sync only).
    /// Does not load from disk; the caller retains the `SyncText` list.
    /// Rebuild the display-only overlay from authoritative texts (idle/open/sync only).
    fn install_text_overlay(&mut self, overlay: TextOverlay) {
        self.text_overlay = overlay;
    }

    fn install_page_links(&mut self, links: Vec<sync_protocol::SyncPageLink>) {
        self.page_links = links;
    }

    fn chrome_damaged(&self) -> bool {
        self.chrome_damaged
    }

    fn note_ink_dirty(&mut self, dirty: Option<DirtyRegion>) {
        if !self.chrome_on_panel {
            // Fullscreen writing: the bands are paper, nothing to repair.
            return;
        }
        if let Some(d) = dirty {
            if self.chrome_layout.damaged_by(d) {
                self.chrome_damaged = true;
            }
        }
    }

    fn full_rect(&self) -> Result<MxcfbRect, String> {
        let full = DirtyRegion::new(0, 0, self.geometry.xres, self.geometry.yres);
        let (x, y, w, h) = dirty_to_send_rect(full, self.geometry.xres, self.geometry.yres)?;
        Ok(MxcfbRect {
            top: y as u32,
            left: x as u32,
            width: w,
            height: h,
        })
    }

    /// GC16 FULL of the whole panel (mode transitions: no ghosted Home/modal remnants).
    fn refresh_full(&mut self) -> Result<(u32, u32), String> {
        let mx = self.full_rect()?;
        let r = self
            .fb
            .send_update_wait_full(mx)
            .map_err(|e| e.to_string())?;
        let sample = self.fb.sample_visible_agreement();
        crate::launch_diag::live().emit_fb_full(
            "gc16_full",
            r.marker,
            r.collision_test,
            r.send_ioctl_us,
            r.wait_ioctl_us,
            mx.left,
            mx.top,
            mx.width,
            mx.height,
            self.fb.last_flush_copy(),
            Some(&sample),
        );
        Ok((r.marker, r.collision_test))
    }

    /// GC16 (non-full) of one UI rect: keyboard typing only.
    fn refresh_rect(&mut self, rect: Rect) -> Result<(), String> {
        let Some(r) = rect.clamp_to(&self.geometry) else {
            return Ok(());
        };
        let mx = MxcfbRect {
            top: r.y,
            left: r.x,
            width: r.w,
            height: r.h,
        };
        self.fb.send_update_wait(mx).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn to_panel(&self, page: PagePoint) -> Option<(i32, i32)> {
        let transform = self.profile.transform().ok()?;
        let (x, y) = transform.to_panel_pixel(page)?;
        Some((x as i32, y as i32))
    }

    pub fn geometry(&self) -> FbGeometry {
        self.geometry
    }

    fn set_profile(&mut self, profile: RenderProfile) {
        self.profile = profile;
    }

    /// Sleep screen (centered Ignite mark), GC16 FULL. Returns the framebuffer
    /// fingerprint the sleep guard compares against.
    fn render_sleep(&mut self) -> Result<u64, String> {
        self.chrome_on_panel = false;
        crate::sleep_ui::render_sleep(self.fb.as_slice_mut(), &self.geometry)?;
        let (marker, collision) = self.refresh_full()?;
        println!("sleep_screen_gc16_full marker={marker} collision={collision}");
        Ok(self.fb_fingerprint())
    }

    fn fb_fingerprint(&self) -> u64 {
        crate::sleep_ui::fingerprint(self.fb.as_slice(), &self.geometry)
    }

    /// Paint Home (+ optional dialog) into the live framebuffer and submit one GC16 FULL.
    /// No notebook background or journal is read or written by this path.
    fn render_home(
        &mut self,
        cards: &[HomeCard],
        folders: &[FolderInfo],
        open_folder: Option<&str>,
        state: &HomeState,
        modal: Option<&Modal>,
    ) -> Result<(), String> {
        self.chrome_on_panel = false;
        let summary = render_home_view(
            self.fb.as_slice_mut(),
            &self.geometry,
            cards,
            folders,
            open_folder,
            state,
        )?;
        if let Some(spec) = modal.and_then(dialog_spec) {
            render_dialog(self.fb.as_slice_mut(), &self.geometry, &spec)?;
        }
        let gc16_t0 = Instant::now();
        let (marker, collision) = self.refresh_full()?;
        let gc16_ms = gc16_t0.elapsed().as_millis();
        println!(
            "home_gc16_full marker={marker} collision={collision} gc16_ms={gc16_ms} folders={} open_folder={} visible_entries={} total_entries={} selected_index={} viewport_page={}/{} modal={}",
            folders.len(),
            open_folder.unwrap_or("-"),
            summary.visible_entries,
            summary.total_entries,
            state.selected_index,
            summary.viewport_page + 1,
            summary.viewport_pages,
            modal.is_some(),
        );
        Ok(())
    }

    /// D1 Sync screen (GC16 FULL).
    fn render_sync(&mut self, view: &crate::server_link::LinkView) -> Result<(), String> {
        self.chrome_on_panel = false;
        let rows = crate::sync_ui::render_sync(self.fb.as_slice_mut(), &self.geometry, view)?;
        let (marker, collision) = self.refresh_full()?;
        println!(
            "sync_gc16_full marker={marker} collision={collision} servers={rows} searching={} phase={:?}",
            view.searching,
            std::mem::discriminant(&view.phase),
        );
        Ok(())
    }

    fn render_tools(&mut self, state: &crate::tools_ui::ToolsState) -> Result<(), String> {
        self.chrome_on_panel = false;
        crate::tools_ui::render_tools(self.fb.as_slice_mut(), &self.geometry, state)?;
        let (marker, collision) = self.refresh_full()?;
        println!("tools_gc16_full marker={marker} collision={collision}");
        Ok(())
    }

    fn render_preferences(
        &mut self,
        action: SideButtonAction,
        dialog: Option<&DialogSpec>,
    ) -> Result<(), String> {
        self.chrome_on_panel = false;
        crate::settings_ui::render_preferences(self.fb.as_slice_mut(), &self.geometry, action)?;
        if let Some(spec) = dialog {
            render_dialog(self.fb.as_slice_mut(), &self.geometry, spec)?;
        }
        let (marker, collision) = self.refresh_full()?;
        println!("preferences_gc16_full marker={marker} collision={collision}");
        Ok(())
    }

    fn render_link_picker(
        &mut self,
        state: &LinkPickerState,
        cards: &[HomeCard],
        dialog: Option<&DialogSpec>,
    ) -> Result<(), String> {
        self.chrome_on_panel = false;
        crate::link_picker::render_link_picker(
            self.fb.as_slice_mut(),
            &self.geometry,
            state,
            cards,
        )?;
        if let Some(spec) = dialog {
            render_dialog(self.fb.as_slice_mut(), &self.geometry, spec)?;
        }
        let (marker, collision) = self.refresh_full()?;
        println!(
            "link_picker_gc16_full marker={marker} collision={collision} stage={:?}",
            std::mem::discriminant(&state.stage)
        );
        Ok(())
    }

    /// Full-screen title keyboard. `region` limits the refresh to the changed area (typing);
    /// `None` submits GC16 FULL (open/close transitions).
    fn render_title_entry(&mut self, modal: &Modal, region: Option<Rect>) -> Result<(), String> {
        let Modal::TitleEntry { purpose, editor } = modal else {
            return Err("internal: render_title_entry without a title modal".into());
        };
        let (heading, submit) = match purpose {
            TitlePurpose::Create => ("NEW NOTEBOOK", "CREATE"),
            TitlePurpose::Rename { .. } => ("RENAME NOTEBOOK", "SAVE"),
            TitlePurpose::ServerUrl => ("SERVER ADDRESS", "CONNECT"),
            TitlePurpose::CreateFolder => ("NEW FOLDER", "CREATE"),
            TitlePurpose::RenameFolder { .. } => ("RENAME FOLDER", "SAVE"),
            TitlePurpose::CreateNotebookInFolder { .. } => ("NEW NOTEBOOK", "CREATE"),
        };
        self.chrome_on_panel = false;
        render_title_editor(
            self.fb.as_slice_mut(),
            &self.geometry,
            heading,
            submit,
            editor,
        )?;
        match region {
            Some(r) => self.refresh_rect(r),
            None => {
                let (marker, collision) = self.refresh_full()?;
                println!("title_entry_gc16_full marker={marker} collision={collision}");
                Ok(())
            }
        }
    }

    /// Durable background + durable strokes + prepared text overlay, then live chrome
    /// while `chrome_visible` (F11.3). No refresh.
    ///
    /// Chrome is framebuffer-only; `background` is never modified here. Hiding chrome is a
    /// full recomposition, so handwriting and text under the bands reappear.
    fn compose_page(
        &mut self,
        background: &[u8],
        strokes: &[Stroke],
        chrome: Option<PageChrome>,
    ) -> Result<(), String> {
        self.page_background = background.to_vec();
        restore_background(&mut self.fb, background)?;
        let geo = self.geometry;
        let overlay = chrome
            .filter(|_| self.chrome_visible)
            .map(|page| (self.chrome_title.as_str(), page));
        let painted = overlay.is_some();
        compose_notebook_layers(
            self.fb.as_slice_mut(),
            &geo,
            &self.profile,
            strokes,
            Some(&self.text_overlay),
            &self.page_links,
            overlay,
        )?;
        self.chrome_on_panel = painted;
        self.chrome_damaged = false;
        Ok(())
    }

    /// Restore background + raster strokes + optional live chrome + GC16 FULL.
    ///
    /// `chrome` is framebuffer UI only — never persisted.
    pub fn reconstruct_page(
        &mut self,
        background: &[u8],
        strokes: &[Stroke],
        chrome: Option<PageChrome>,
    ) -> Result<(u64, u64), String> {
        let t0 = Instant::now();
        self.compose_page(background, strokes, chrome)?;
        let reconstruct_us = t0.elapsed().as_micros() as u64;
        let t1 = Instant::now();
        let mx = self.full_rect()?;
        let r = self
            .fb
            .send_update_wait_full(mx)
            .map_err(|e| e.to_string())?;
        let gc16_us = t1.elapsed().as_micros() as u64;
        println!(
            "reconstruct_gc16_full marker={} collision={} send_ioctl_us={} wait_ioctl_us={} reconstruct_us={} gc16_us={}",
            r.marker, r.collision_test, r.send_ioctl_us, r.wait_ioctl_us, reconstruct_us, gc16_us
        );
        Ok((reconstruct_us, gc16_us))
    }

    /// Notebook view with chrome and an optional dialog overlay, GC16 FULL.
    fn present_notebook(
        &mut self,
        background: &[u8],
        strokes: &[Stroke],
        chrome: PageChrome,
        modal: Option<&Modal>,
    ) -> Result<(), String> {
        self.compose_page(background, strokes, Some(chrome))?;
        if let Some(spec) = modal.and_then(dialog_spec) {
            render_dialog(self.fb.as_slice_mut(), &self.geometry, &spec)?;
        }
        let (marker, collision) = self.refresh_full()?;
        println!(
            "notebook_present_gc16_full marker={marker} collision={collision} page=\"{}\" modal={} chrome_visible={}",
            chrome.label(),
            modal.is_some(),
            self.chrome_on_panel
        );
        Ok(())
    }

    /// Repaint chrome bands after live ink crossed them (idle + disarmed only).
    fn repair_chrome(&mut self, chrome: PageChrome) -> Result<(), String> {
        if !self.chrome_on_panel {
            self.chrome_damaged = false;
            return Ok(());
        }
        let layout = render_notebook_chrome(
            self.fb.as_slice_mut(),
            &self.geometry,
            &self.chrome_title,
            chrome,
        )?;
        self.chrome_damaged = false;
        self.refresh_rect(layout.top_band)?;
        self.refresh_rect(layout.bottom_band)?;
        println!("chrome_repaired page=\"{}\"", chrome.label());
        Ok(())
    }

    fn canonical_rect_dirty(
        &self,
        rect: crate::content::CanonicalRect,
        pad_px: i32,
    ) -> Option<DirtyRegion> {
        let xf = self.profile.transform().ok()?;
        let points = [
            PagePoint::new(rect.x, rect.y),
            PagePoint::new(rect.x + rect.width, rect.y),
            PagePoint::new(rect.x, rect.y + rect.height),
            PagePoint::new(rect.x + rect.width, rect.y + rect.height),
        ];
        let mut panel = Vec::with_capacity(4);
        for point in points {
            let (x, y) = xf.to_panel_pixel(point)?;
            panel.push((x as f32, y as f32));
        }
        DirtyRegion::from_points(&panel, pad_px)
    }

    fn canonical_rect_panel(&self, rect: crate::content::CanonicalRect) -> Option<Rect> {
        let dirty = self
            .canonical_rect_dirty(rect, 8)?
            .clamp_to(self.geometry.xres, self.geometry.yres)?;
        Some(Rect::new(
            dirty.x as u32,
            dirty.y as u32,
            dirty.width,
            dirty.height,
        ))
    }

    fn touch_du(&mut self, dirty: DirtyRegion) -> Result<(), String> {
        let (x, y, w, h) = dirty_to_send_rect(dirty, self.geometry.xres, self.geometry.yres)?;
        let rect = MxcfbRect {
            top: y as u32,
            left: x as u32,
            width: w,
            height: h,
        };
        self.fb
            .send_update_wetink_du(rect)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn reset_lasso_dash(&mut self) {
        self.lasso_dash_phase = 0;
        self.lasso_dash_started = false;
    }

    /// Append one transient lasso segment and submit only that small DU region.
    /// A dash-off gap may write no pixels; then no DU is sent.
    fn draw_lasso_segment(
        &mut self,
        from: Point,
        to: Point,
    ) -> Result<Option<DirtyRegion>, String> {
        const LASSO_DASH_ON_PX: u32 = 3;
        const LASSO_DASH_OFF_PX: u32 = 5;
        let Some((x0, y0)) = self.to_panel(from.page_point()) else {
            return Err("lasso point outside display transform".into());
        };
        let Some((x1, y1)) = self.to_panel(to.page_point()) else {
            return Err("lasso point outside display transform".into());
        };
        let skip_start = self.lasso_dash_started;
        let painted = raster_dashed_line(
            self.fb.as_slice_mut(),
            &self.geometry,
            x0,
            y0,
            x1,
            y1,
            BLACK,
            1,
            LASSO_DASH_ON_PX,
            LASSO_DASH_OFF_PX,
            skip_start,
            &mut self.lasso_dash_phase,
        )
        .map_err(|e| format!("{e:?}"))?;
        self.lasso_dash_started = true;
        let Some(dirty) = painted.dirty else {
            return Ok(None);
        };
        self.touch_du(dirty)?;
        Ok(Some(dirty))
    }

    fn draw_selection_box(&mut self, bounds: crate::content::CanonicalRect) -> Result<(), String> {
        let Some(rect) = self.canonical_rect_panel(bounds) else {
            return Ok(());
        };
        let x0 = rect.x as i32;
        let y0 = rect.y as i32;
        let x1 = rect.right().saturating_sub(1) as i32;
        let y1 = rect.bottom().saturating_sub(1) as i32;
        for (a, b) in [
            ((x0, y0), (x1, y0)),
            ((x1, y0), (x1, y1)),
            ((x1, y1), (x0, y1)),
            ((x0, y1), (x0, y0)),
        ] {
            raster_line(
                self.fb.as_slice_mut(),
                &self.geometry,
                a.0,
                a.1,
                b.0,
                b.1,
                BLACK,
                1,
            )
            .map_err(|e| format!("{e:?}"))?;
        }
        Ok(())
    }

    /// Restore the prior transient area from durable layers, optionally translate selected
    /// strokes for a live preview, draw the lightweight selection box, then submit a regional
    /// DU or clean GC16. No framebuffer capture becomes document state.
    #[allow(clippy::too_many_arguments)]
    fn present_touch_selection(
        &mut self,
        strokes: &[Stroke],
        restore: Option<DirtyRegion>,
        bounds: crate::content::CanonicalRect,
        moved_ids: &HashSet<String>,
        dx: f32,
        dy: f32,
        clean: bool,
        diag_label: &str,
    ) -> Result<DirtyRegion, String> {
        let t0 = Instant::now();
        let shown = crate::content::CanonicalRect {
            x: bounds.x + dx,
            y: bounds.y + dy,
            ..bounds
        };
        let box_dirty = self
            .canonical_rect_dirty(shown, 12)
            .ok_or("selection bounds outside display transform")?;
        let restore_was = restore.is_some();
        // Paint previous footprint ∪ current box. The caller stores `box_dirty` (current),
        // not this union, as the next preview footprint.
        let dirty = restore.map_or(box_dirty, |old| old.union(box_dirty));
        let dirty_calc_us = t0.elapsed().as_micros() as u64;
        let dirty_pixels = u64::from(dirty.width).saturating_mul(u64::from(dirty.height));
        let t_recompose = Instant::now();
        let stats = if moved_ids.is_empty() || (dx == 0.0 && dy == 0.0) {
            recompose_dirty_notebook_layers(
                self.fb.as_slice_mut(),
                &self.geometry,
                &self.profile,
                &self.page_background,
                strokes,
                Some(&self.text_overlay),
                &self.page_links,
                dirty,
            )?
        } else {
            recompose_dirty_notebook_layers_translated(
                self.fb.as_slice_mut(),
                &self.geometry,
                &self.profile,
                &self.page_background,
                strokes,
                Some(&self.text_overlay),
                &self.page_links,
                dirty,
                moved_ids,
                dx,
                dy,
            )?
        };
        let recompose_us = t_recompose.elapsed().as_micros() as u64;
        let t_box = Instant::now();
        self.draw_selection_box(shown)?;
        let selection_box_us = t_box.elapsed().as_micros() as u64;
        let (x, y, w, h) = dirty_to_send_rect(dirty, self.geometry.xres, self.geometry.yres)?;
        let rect = MxcfbRect {
            top: y as u32,
            left: x as u32,
            width: w,
            height: h,
        };
        let t_du = Instant::now();
        let (du_submit_us, du_marker, du_ioctl_us, du_waited) = if clean {
            let r = self.fb.send_update_wait(rect).map_err(|e| e.to_string())?;
            (
                t_du.elapsed().as_micros() as u64,
                r.marker,
                r.send_ioctl_us,
                Some(r.wait_ioctl_us),
            )
        } else {
            let r = self
                .fb
                .send_update_wetink_du(rect)
                .map_err(|e| e.to_string())?;
            (
                t_du.elapsed().as_micros() as u64,
                r.marker,
                r.send_ioctl_us,
                None,
            )
        };
        let total_preview_us = t0.elapsed().as_micros() as u64;
        let selected_point_count: usize = strokes
            .iter()
            .filter(|s| moved_ids.contains(s.id.as_str()))
            .map(|s| s.points.len())
            .sum();
        println!(
            "lasso_preview_diag label={diag_label} dirty_x={} dirty_y={} dirty_width={} dirty_height={} dirty_pixels={dirty_pixels} \
             box_w={} box_h={} restore_was={restore_was} page_stroke_count={} selected_fragment_count={} selected_point_count={selected_point_count} \
             strokes_considered={} replayed_stroke_count={} scratch_bytes={} \
             dirty_calculation_us={dirty_calc_us} recompose_us={recompose_us} selection_box_us={selection_box_us} \
             du_submit_us={du_submit_us} du_send_ioctl_us={du_ioctl_us} du_wait_ioctl_us={du_waited:?} du_marker={du_marker} \
             du_waited={} clean={clean} total_preview_us={total_preview_us} dx={dx} dy={dy}",
            dirty.x,
            dirty.y,
            dirty.width,
            dirty.height,
            box_dirty.width,
            box_dirty.height,
            strokes.len(),
            moved_ids.len(),
            stats.strokes_considered,
            stats.strokes_replayed,
            stats.scratch_bytes,
            du_waited.is_some(),
        );
        Ok(box_dirty)
    }

    fn clear_touch_overlay(
        &mut self,
        strokes: &[Stroke],
        dirty: Option<DirtyRegion>,
    ) -> Result<(), String> {
        let Some(dirty) = dirty else { return Ok(()) };
        recompose_dirty_notebook_layers(
            self.fb.as_slice_mut(),
            &self.geometry,
            &self.profile,
            &self.page_background,
            strokes,
            Some(&self.text_overlay),
            &self.page_links,
            dirty,
        )?;
        let (x, y, w, h) = dirty_to_send_rect(dirty, self.geometry.xres, self.geometry.yres)?;
        self.fb
            .send_update_wait(MxcfbRect {
                top: y as u32,
                left: x as u32,
                width: w,
                height: h,
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl InkTransport for KindleTransport {
    fn now_us(&self) -> u64 {
        self.wall.now_us()
    }

    fn arm(&mut self) -> Result<(), String> {
        let r = self
            .fb
            .set_stylus_mode(stylus_mode::WITH_NO_TPS)
            .map_err(|e| e.to_string())?;
        self.armed = true;
        println!(
            "stylus_mode_enable=1 ioctl_rc={} set_stylus_mode_ioctl_us={}",
            r.ioctl_rc, r.set_stylus_mode_ioctl_us
        );
        Ok(())
    }

    fn disarm(&mut self) -> Result<(), String> {
        let r = self
            .fb
            .set_stylus_mode(stylus_mode::DISABLED)
            .map_err(|e| e.to_string())?;
        self.armed = false;
        println!(
            "stylus_mode_disable=0 ioctl_rc={} set_stylus_mode_ioctl_us={}",
            r.ioctl_rc, r.set_stylus_mode_ioctl_us
        );
        Ok(())
    }

    fn stamp(
        &mut self,
        page: PagePoint,
        pressure: Option<f32>,
    ) -> Result<Option<DirtyRegion>, String> {
        let Some((px, py)) = self.to_panel(page) else {
            return Ok(None);
        };
        let g = self.geometry;
        let ink = self.profile.ink_byte;
        // F6: when the page profile is pressure-capable, all live strokes are pressure-width.
        let painted = match self.profile.pressure {
            Some(curve) => {
                let radius = crate::render::radius_for_pressure(curve, pressure);
                raster_stamp(self.fb.as_slice_mut(), &g, px, py, ink, radius)
                    .map_err(|e| format!("{e:?}"))?
            }
            None => raster_stamp(
                self.fb.as_slice_mut(),
                &g,
                px,
                py,
                ink,
                self.profile.brush_radius,
            )
            .map_err(|e| format!("{e:?}"))?,
        };
        self.note_ink_dirty(painted.dirty);
        Ok(painted.dirty)
    }

    fn segment(
        &mut self,
        from: PagePoint,
        pa: Option<f32>,
        to: PagePoint,
        pb: Option<f32>,
    ) -> Result<Option<DirtyRegion>, String> {
        let Some((x0, y0)) = self.to_panel(from) else {
            return Ok(None);
        };
        let Some((x1, y1)) = self.to_panel(to) else {
            return Ok(None);
        };
        let g = self.geometry;
        let ink = self.profile.ink_byte;
        // F6: pressure-capable profile → per-sample width shared with reconstruction.
        let painted = match self.profile.pressure {
            Some(curve) => crate::render::raster_pressure_segment_panel(
                self.fb.as_slice_mut(),
                &g,
                curve,
                ink,
                (x0, y0),
                pa,
                (x1, y1),
                pb,
            )?,
            None => raster_line(
                self.fb.as_slice_mut(),
                &g,
                x0,
                y0,
                x1,
                y1,
                ink,
                self.profile.brush_radius,
            )
            .map_err(|e| format!("{e:?}"))?,
        };
        self.note_ink_dirty(painted.dirty);
        Ok(painted.dirty)
    }

    fn send_du(&mut self, dirty: DirtyRegion) -> Result<DuSendResult, String> {
        let (x, y, w, h) = dirty_to_send_rect(dirty, self.geometry.xres, self.geometry.yres)?;
        let mx = MxcfbRect {
            top: y as u32,
            left: x as u32,
            width: w,
            height: h,
        };
        let r = self
            .fb
            .send_update_wetink_du(mx)
            .map_err(|e| e.to_string())?;
        Ok(DuSendResult {
            marker: r.marker,
            send_ioctl_us: r.send_ioctl_us,
        })
    }

    fn wait_marker(&mut self, marker: u32) -> Result<WaitResult, String> {
        let w = self.fb.wait_for_update(marker).map_err(|e| e.to_string())?;
        Ok(WaitResult {
            marker: w.marker,
            collision_test: w.collision_test,
            wait_ioctl_us: w.wait_ioctl_us,
        })
    }

    fn commit_gc16(&mut self, dirty: DirtyRegion, strokes: &[Stroke]) -> Result<(), String> {
        let stats = recompose_dirty_notebook_layers(
            self.fb.as_slice_mut(),
            &self.geometry,
            &self.profile,
            &self.page_background,
            strokes,
            Some(&self.text_overlay),
            &self.page_links,
            dirty,
        )?;
        let (x, y, w, h) = dirty_to_send_rect(dirty, self.geometry.xres, self.geometry.yres)
            .map_err(|e| format!("GC16 {e}"))?;
        let mx = MxcfbRect {
            top: y as u32,
            left: x as u32,
            width: w,
            height: h,
        };
        let r = self.fb.send_update_wait(mx).map_err(|e| e.to_string())?;
        println!(
            "gc16_commit marker={} collision={} send_ioctl_us={} wait_ioctl_us={} dirty=({x},{y} {w}x{h}) strokes_replayed={}/{} scratch_bytes={}",
            r.marker, r.collision_test, r.send_ioctl_us, r.wait_ioctl_us,
            stats.strokes_replayed, stats.strokes_considered, stats.scratch_bytes
        );
        Ok(())
    }
}

impl Drop for KindleTransport {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.fb.set_stylus_mode(stylus_mode::DISABLED);
            self.armed = false;
        }
    }
}

fn device_to_page(cal: &DeviceCalibration, x: f32, y: f32) -> PagePoint {
    cal.to_page(DevicePoint::new(x, y))
}

pub struct RuntimeConfig {
    pub fb_path: String,
    pub stylus_path: String,
    pub ink_request: InkRequest,
    pub du_interval_us: u64,
    pub du_override: bool,
    pub confirm_write: bool,
    pub data_dir: PathBuf,
    /// Developer: log Down/Up/StreamGap inter-stroke dispatch diagnostics.
    pub trace_interstroke: bool,
    /// Developer: per completed stroke pressure sample summary (F6 characterization).
    pub trace_pressure: bool,
    /// F11.1: read screensaver invalidate/restore lines relayed by `scribe-lifecycle` on stdin.
    pub display_events_stdin: bool,
    /// F11.2: read-only touchscreen observer (never EVIOCGRAB). `None` = pen-only.
    pub touch_path: Option<String>,
    /// S2B: outbound sync state directory (`--sync-dir`). `None` = sync off (default).
    pub sync_dir: Option<PathBuf>,
    /// D1 product mode: paired-server device configuration (`--device-dir`, outside every
    /// library). With `--sync-dir` and no developer `config.json`, sync connects to the paired
    /// Scribe Server and Home shows SYNC.
    pub device_dir: Option<PathBuf>,
}

/// Load `texts.jsonl` + `links.jsonl` and derive the overlay from that same texts vec.
/// Inbound, navigate, and notebook present all go through this so the overlay cannot stay
/// on a previous page's texts while the controller holds a new list.
fn load_and_paint_texts(
    transport: &mut KindleTransport,
    notebook_dir: &Path,
    page_id: &str,
) -> Result<Vec<sync_protocol::SyncText>, String> {
    let (texts, _) = load_and_paint_page_content(transport, notebook_dir, page_id)?;
    Ok(texts)
}

fn load_and_paint_page_content(
    transport: &mut KindleTransport,
    notebook_dir: &Path,
    page_id: &str,
) -> Result<
    (
        Vec<sync_protocol::SyncText>,
        Vec<sync_protocol::SyncPageLink>,
    ),
    String,
> {
    let xf = transport.profile.transform()?;
    let (texts, overlay) = crate::text_paint::load_texts_and_overlay(
        &page_dir(notebook_dir, page_id),
        &transport.geometry,
        xf,
    )?;
    transport.install_text_overlay(overlay);
    let links = crate::store::load_page_links(&page_dir(notebook_dir, page_id))?;
    transport.install_page_links(links.clone());
    Ok((texts, links))
}

/// Install the library-authoritative active notebook into the unchanged page runtime.
/// Any error here is post-commit for switch/create/delete callers and must be fatal; this
/// helper never rolls `library.meta` back.
struct ActiveNotebookBindings<'a> {
    notebook_dir: &'a mut PathBuf,
    manifest: &'a mut PersistedNotebookManifestV1,
    profile: &'a mut RenderProfile,
    background: &'a mut Vec<u8>,
}

fn install_opened_notebook(
    active_notebook_id: &str,
    title: &str,
    new_notebook_dir: PathBuf,
    opened: NotebookOpen,
    ctrl: &mut KindleCtrl,
    bindings: ActiveNotebookBindings<'_>,
) -> Result<(), String> {
    if opened.manifest.notebook_id != active_notebook_id {
        return Err(format!(
            "FATAL: active notebook identity mismatch library={} notebook.meta={}",
            active_notebook_id, opened.manifest.notebook_id
        ));
    }
    let page_id = opened.page_id.clone();
    let chrome = chrome_for_page(&opened.manifest, &page_id)?;
    let label = chrome.label();
    let profile = opened.loaded.render_profile.clone();
    let strokes = opened.loaded.strokes.clone();
    let roots = opened.loaded.root_of.clone();
    let background = opened.loaded.background.clone();
    ctrl.transport_mut().set_profile(profile.clone());
    ctrl.transport_mut().set_chrome_title(title);
    let (texts, links) =
        load_and_paint_page_content(ctrl.transport_mut(), &new_notebook_dir, &page_id)
            .map_err(|e| format!("FATAL: load page texts: {e}"))?;
    ctrl.transport_mut()
        .reconstruct_page(&background, &strokes, Some(chrome))
        .map_err(|e| {
            format!("FATAL: active notebook committed but framebuffer reconstruct failed: {e}")
        })?;
    ctrl.bind_page_with_roots(
        BoundPageKey::new(active_notebook_id, &page_id),
        strokes,
        roots,
        RuntimeStore::Bound(opened.store),
        texts,
        links,
    )
    .map_err(|e| format!("FATAL: active notebook committed but page bind failed: {e}"))?;
    *bindings.notebook_dir = new_notebook_dir;
    *bindings.manifest = opened.manifest;
    *bindings.profile = profile;
    *bindings.background = background;
    ctrl.clear_pending_clean();
    if !opened.orphan_dirs.is_empty() {
        println!("orphan_page_dirs={:?}", opened.orphan_dirs);
    }
    println!(
        "notebook_loaded notebook={} active_page={} hud=\"{}\" page_count={} strokes={}",
        active_notebook_id,
        page_id,
        label,
        bindings.manifest.page_ids.len(),
        ctrl.session.committed_count()
    );
    Ok(())
}

fn finish_shutdown(ctrl: &mut KindleCtrl, stop_requested: bool) -> (i32, bool, bool) {
    match ctrl.shutdown() {
        Ok(()) => {
            let code = if stop_requested { 0 } else { 1 };
            (code, true, false)
        }
        Err(e) => {
            eprintln!("error: shutdown: {e}");
            println!("shutdown_safe=false");
            println!("stylus_mode_disarm_failed=true");
            (1, false, true)
        }
    }
}

fn present_loading_splash(
    fb: &mut MappedFramebuffer,
    geo: &FbGeometry,
) -> Result<(u32, u32), String> {
    crate::sleep_ui::render_startup(fb.as_slice_mut(), geo)?;
    // GC16 FULL: the previous panel is Amazon UI (light). A partial/DU refresh of a dark
    // splash would ghost that chrome. The frame itself is black, so the full refresh is not
    // a white flash. One wait, then nothing until Home.
    let full = DirtyRegion::new(0, 0, geo.xres, geo.yres);
    let (x, y, w, h) = dirty_to_send_rect(full, geo.xres, geo.yres)?;
    let mx = MxcfbRect {
        top: y as u32,
        left: x as u32,
        width: w,
        height: h,
    };
    let r = fb.send_update_wait_full(mx).map_err(|e| e.to_string())?;
    let sample = fb.sample_visible_agreement();
    crate::launch_diag::live().emit_fb_full(
        "loading_splash",
        r.marker,
        r.collision_test,
        r.send_ioctl_us,
        r.wait_ioctl_us,
        mx.left,
        mx.top,
        mx.width,
        mx.height,
        fb.last_flush_copy(),
        Some(&sample),
    );
    Ok((r.marker, r.collision_test))
}

/// Process exits; Amazon/Scriptlet reclaims the panel. Do not leave `Loading...` without a log.
fn abort_after_loading(detail: impl std::fmt::Display) -> i32 {
    eprintln!("startup_failed after_loading=true detail={detail}");
    println!("startup_failed=true");
    1
}

fn restore_background(fb: &mut MappedFramebuffer, pixels: &[u8]) -> Result<(), String> {
    let g = fb.geometry;
    if pixels.len() == (g.xres as usize) * (g.yres as usize) {
        fb.restore_rect(0, 0, g.xres, g.yres, pixels)
            .map_err(|e| e.to_string())
    } else if pixels.len() == (g.line_length as usize) * (g.yres as usize) {
        fb.as_slice_mut()[..pixels.len()].copy_from_slice(pixels);
        Ok(())
    } else {
        Err(format!(
            "background len {} matches neither {} nor {}",
            pixels.len(),
            g.xres * g.yres,
            g.line_length * g.yres
        ))
    }
}

fn chrome_for_page(
    manifest: &PersistedNotebookManifestV1,
    page_id: &str,
) -> Result<PageChrome, String> {
    page_chrome_for(manifest, page_id)
}

/// Commit provisional partial-erase on Eraser Up (or report empty). Fatal on persist/recon fail.
fn eraser_commit_gesture(
    ctrl: &mut KindleCtrl,
    current_bg: &[u8],
    chrome: PageChrome,
    commit: &crate::eraser::PartialEraseCommit,
) -> Result<(), String> {
    ctrl.totals.partial_erase_gestures += 1;
    if !commit.dirty {
        ctrl.totals.eraser_blank_gestures += 1;
        ctrl.totals.partial_erase_empty_gestures += 1;
        println!(
            "partial_erase_gesture sweeps={} source_strokes_touched={} \
             source_strokes_fully_deleted={} fragments_created={} replacement_targets=0 \
             reconstruct=false",
            commit.stats.sweeps,
            commit.stats.sources_touched,
            commit.stats.sources_fully_deleted,
            commit.stats.fragments_created,
        );
        return Ok(());
    }
    let t0 = Instant::now();
    let timing = ctrl
        .durable_replace_strokes(&commit.replacements)
        .map_err(|e| {
            // persistence_errors already incremented by durable_replace_strokes.
            ctrl.totals.partial_erase_persistence_errors += 1;
            format!("partial_erase fatal=true persist_failed=true: {e}")
        })?;
    let persist_append_us = timing.append_us;
    let persist_fsync_us = timing.fsync_us;
    ctrl.totals.eraser_unique_strokes_deleted += commit.stats.sources_fully_deleted;
    ctrl.totals.strokes_deleted += commit.stats.sources_fully_deleted;
    ctrl.totals.partial_erase_sources_touched += commit.stats.sources_touched;
    ctrl.totals.partial_erase_full_deletes += commit.stats.sources_fully_deleted;
    ctrl.totals.partial_erase_fragments_created += commit.stats.fragments_created;

    let remaining: Vec<Stroke> = ctrl.session.committed_strokes().to_vec();
    match ctrl
        .transport_mut()
        .reconstruct_page(current_bg, &remaining, Some(chrome))
    {
        Ok((reconstruct_us, gc16_us)) => {
            ctrl.totals.eraser_reconstructs += 1;
            ctrl.clear_pending_clean();
            let _ = t0;
            println!(
                "partial_erase_gesture sweeps={} source_strokes_touched={} \
                 source_strokes_fully_deleted={} fragments_created={} replacement_targets={} \
                 persist_append_us={persist_append_us} persist_fsync_us={persist_fsync_us} \
                 reconstruct_us={reconstruct_us} gc16_us={gc16_us}",
                commit.stats.sweeps,
                commit.stats.sources_touched,
                commit.stats.sources_fully_deleted,
                commit.stats.fragments_created,
                commit.replacements.len(),
            );
            Ok(())
        }
        Err(e) => {
            ctrl.totals.eraser_reconstruct_failures += 1;
            ctrl.totals.partial_erase_reconstruct_failures += 1;
            Err(format!(
                "partial_erase fatal=true panel_reconstruct=false (durable edit kept): {e}"
            ))
        }
    }
}

/// Perform a page switch/create. Returns Ok(true) if switched, Ok(false) if no-op.
fn navigate(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    ctrl: &mut KindleCtrl,
    notebook_profile: &RenderProfile,
    current_bg: &mut Vec<u8>,
    action: NavDownAction,
) -> Result<bool, String> {
    if !ctrl.is_idle() || ctrl.life().is_armed() {
        // Mid-stroke: ignore navigation.
        return Ok(false);
    }

    let from = ctrl.session.page_id().to_string();
    let from_idx = page_index(manifest, &from).unwrap_or(0);
    let page_count = manifest.page_ids.len();

    let (target_id, direction, creating) = match action {
        NavDownAction::UndoLastStroke => {
            return Err("internal: UndoLastStroke must not enter navigate".into());
        }
        NavDownAction::IgnoreNoInk => {
            return Err("internal: IgnoreNoInk must not enter navigate".into());
        }
        NavDownAction::Prev => {
            if from_idx == 0 {
                return Ok(false);
            }
            (
                manifest.page_ids[from_idx - 1].clone(),
                PageSwitchDirection::Prev,
                false,
            )
        }
        NavDownAction::NextOrCreate => {
            if from_idx + 1 < page_count {
                (
                    manifest.page_ids[from_idx + 1].clone(),
                    PageSwitchDirection::Next,
                    false,
                )
            } else if !crate::nav::may_create_page_on_right_nav(ctrl.store().has_authored_history())
            {
                println!(
                    "nav_create_refused reason=pristine_trailing_page page={from} page_count={page_count}"
                );
                return Ok(false);
            } else {
                (allocate_page_id(manifest), PageSwitchDirection::New, true)
            }
        }
    };

    let geo = ctrl.transport().geometry();
    // Snapshot current for repair.
    let repair_bg = current_bg.clone();
    let repair_strokes = ctrl.session.committed_strokes().to_vec();

    let (new_store, loaded, created_now) = if creating {
        match create_page_files_from_template(data_dir, &target_id, DEFAULT_PAGE_ID) {
            Ok(v) => (v.0, v.1, true),
            Err(e) => {
                ctrl.totals.page_switch_failures += 1;
                return Err(format!("page create failed (staying on {from}): {e}"));
            }
        }
    } else {
        match load_page_for_switch(data_dir, &target_id, notebook_profile, &geo) {
            Ok(v) => (v.0, v.1, false),
            Err(e) => {
                ctrl.totals.page_switch_failures += 1;
                eprintln!("page_switch_failed validation (staying on {from}): {e}");
                return Ok(false);
            }
        }
    };

    let target_strokes = loaded.strokes.clone();
    let target_roots = loaded.root_of.clone();
    let target_bg = loaded.background.clone();
    let target_count = target_strokes.len();

    // Label must reflect post-create page_count before reconstruct paints chrome.
    let target_index_0 = if creating {
        page_count // new page appends at end
    } else if matches!(direction, PageSwitchDirection::Prev) {
        from_idx - 1
    } else {
        from_idx + 1
    };
    let label_count = if creating { page_count + 1 } else { page_count };
    let target_chrome = PageChrome {
        index: target_index_0 + 1,
        count: label_count,
    };
    let repair_chrome = PageChrome {
        index: from_idx + 1,
        count: page_count,
    };

    // Framebuffer now diverges until reconstruct succeeds.
    let (target_texts, target_links) =
        load_and_paint_page_content(ctrl.transport_mut(), data_dir, &target_id)
            .map_err(|e| format!("page switch load texts failed (staying on {from}): {e}"))?;
    let (reconstruct_us, gc16_us) = match ctrl.transport_mut().reconstruct_page(
        &target_bg,
        &target_strokes,
        Some(target_chrome),
    ) {
        Ok(t) => t,
        Err(e) => {
            ctrl.totals.page_switch_failures += 1;
            eprintln!("page_switch reconstruct failed: {e}; repairing current page");
            let _ = load_and_paint_texts(ctrl.transport_mut(), data_dir, &from);
            if let Err(re) = ctrl.transport_mut().reconstruct_page(
                &repair_bg,
                &repair_strokes,
                Some(repair_chrome),
            ) {
                return Err(format!(
                    "FATAL: target reconstruct failed and current repair failed: {re}"
                ));
            }
            return Ok(false);
        }
    };

    if let Err(e) = ctrl.bind_page_with_roots(
        BoundPageKey::new(&manifest.notebook_id, &target_id),
        target_strokes,
        target_roots,
        RuntimeStore::Bound(new_store),
        target_texts,
        target_links,
    ) {
        ctrl.totals.page_switch_failures += 1;
        // Try repair current model (still old store if bind failed before replace...).
        // bind_page replaces store only on success; on error store unchanged.
        let _ = load_and_paint_texts(ctrl.transport_mut(), data_dir, &from);
        let _ =
            ctrl.transport_mut()
                .reconstruct_page(&repair_bg, &repair_strokes, Some(repair_chrome));
        return Err(format!("bind_page failed: {e}"));
    }

    *current_bg = target_bg;

    let manifest_update_us = if created_now {
        match commit_new_page_to_manifest(data_dir, manifest, &target_id) {
            Ok(us) => {
                ctrl.totals.pages_created += 1;
                let idx = page_index(manifest, &target_id).unwrap_or(0);
                let after = idx.checked_sub(1).map(|i| manifest.page_ids[i].as_str());
                crate::sync_outbound::with_global(|s| {
                    s.page_created(data_dir, &manifest.notebook_id, &target_id, after)
                });
                us
            }
            Err(e) => {
                return Err(format!(
                    "FATAL: visual/model on {target_id} but manifest update failed: {e}"
                ));
            }
        }
    } else {
        match persist_active_page(data_dir, manifest, &target_id) {
            Ok(us) => us,
            Err(e) => {
                return Err(format!(
                    "FATAL: switched to {target_id} but active_page persist failed: {e}"
                ));
            }
        }
    };

    ctrl.totals.page_switches += 1;
    let target_index = page_index(manifest, &target_id).unwrap_or(0);
    let dir_s = match direction {
        PageSwitchDirection::Prev => "prev",
        PageSwitchDirection::Next => "next",
        PageSwitchDirection::New => "new",
    };
    println!(
        "page_switch from={from} to={target_id} direction={dir_s} target_index={target_index} page_count={} target_strokes={target_count} reconstruct_us={reconstruct_us} gc16_us={gc16_us} manifest_update_us={manifest_update_us}",
        manifest.page_ids.len()
    );
    Ok(true)
}

/// Durable delete of the current page (F8). Returns Ok(true) if deleted, Ok(false) if refused.
///
/// After durable commit, binds/reconstructs the successor. Reconstruct failure after commit
/// is fatal (deletion remains authoritative).
fn delete_current_page(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    ctrl: &mut KindleCtrl,
    notebook_profile: &RenderProfile,
    current_bg: &mut Vec<u8>,
) -> Result<bool, String> {
    if !ctrl.is_idle() || ctrl.life().is_armed() {
        println!("page_delete_ignored reason=not_idle");
        return Ok(false);
    }
    let page_id = ctrl.session.page_id().to_string();
    if manifest.page_ids.len() < 2 {
        ctrl.totals.page_delete_refusals += 1;
        println!("page_delete_refused reason=only_page page={page_id}");
        return Ok(false);
    }

    let ticket = sync_prepare(|s| s.prepare_delete_page(&manifest.notebook_id, &page_id))?;
    let commit =
        match delete_page_durable(data_dir, manifest, &page_id, &DeletePageHooks::default()) {
            Ok(c) => {
                sync_resolve(ticket, true);
                c
            }
            Err(e) if e.starts_with("REFUSE PAGE DELETE") => {
                sync_resolve(ticket, false);
                ctrl.totals.page_delete_refusals += 1;
                println!("page_delete_refused detail={e}");
                return Ok(false);
            }
            Err(e) => {
                ctrl.totals.page_delete_failures += 1;
                return Err(e);
            }
        };
    ctrl.totals.pages_deleted += 1;
    let trash_s = match &commit.quarantine {
        QuarantineOutcome::Moved { trash_entry_name } => trash_entry_name.clone(),
        QuarantineOutcome::LeftAsOrphan { reason } => format!("orphan({reason})"),
    };
    println!(
        "page_deleted page={} successor={} quarantine={} manifest_persist_us={} page_count={}",
        commit.deleted_page_id,
        commit.successor_page_id,
        trash_s,
        commit.manifest_persist_us,
        manifest.page_ids.len()
    );

    let geo = ctrl.transport().geometry();
    let (new_store, loaded) =
        match load_page_for_switch(data_dir, &commit.successor_page_id, notebook_profile, &geo) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "FATAL: page delete durable but successor load failed: {e}"
                ));
            }
        };
    let chrome = chrome_for_page(manifest, &commit.successor_page_id)?;
    let label = chrome.label();
    let strokes = loaded.strokes.clone();
    let roots = loaded.root_of.clone();
    let bg = loaded.background.clone();
    let (texts, links) =
        load_and_paint_page_content(ctrl.transport_mut(), data_dir, &commit.successor_page_id)
            .map_err(|e| format!("FATAL: page delete durable but successor texts failed: {e}"))?;
    if let Err(e) = ctrl
        .transport_mut()
        .reconstruct_page(&bg, &strokes, Some(chrome))
    {
        return Err(format!(
            "FATAL: page delete durable but successor reconstruct failed: {e}"
        ));
    }
    if let Err(e) = ctrl.bind_page_with_roots(
        BoundPageKey::new(&manifest.notebook_id, &commit.successor_page_id),
        strokes,
        roots,
        RuntimeStore::Bound(new_store),
        texts,
        links,
    ) {
        return Err(format!(
            "FATAL: page delete durable but bind successor failed: {e}"
        ));
    }
    *current_bg = bg;
    ctrl.clear_pending_clean();
    println!(
        "page_delete_active={} hud=\"{}\" strokes={}",
        commit.successor_page_id,
        label,
        ctrl.session.committed_count()
    );
    Ok(true)
}

/// F9: durable reorder of the current page. Reconstructs in place with updated HUD.
fn move_current_page(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    ctrl: &mut KindleCtrl,
    current_bg: &[u8],
    direction: MovePageDirection,
) -> Result<bool, String> {
    if !ctrl.is_idle() || ctrl.life().is_armed() {
        println!("page_move_ignored reason=not_idle");
        return Ok(false);
    }
    let page_id = ctrl.session.page_id().to_string();
    let dir_s = match direction {
        MovePageDirection::Left => "left",
        MovePageDirection::Right => "right",
    };
    let ticket = match crate::notebook::plan_move_page(manifest, &page_id, direction) {
        Ok((planned, _, to)) => {
            let after = to.checked_sub(1).map(|i| planned.page_ids[i].clone());
            sync_prepare(|s| {
                s.prepare_move_page(&manifest.notebook_id, &page_id, after.as_deref())
            })?
        }
        Err(_) => None,
    };
    let commit = match move_page_durable(
        data_dir,
        manifest,
        &page_id,
        direction,
        &MovePageHooks::default(),
    ) {
        Ok(c) => {
            sync_resolve(ticket, true);
            c
        }
        Err(e) if e.starts_with("REFUSE PAGE MOVE") => {
            sync_resolve(ticket, false);
            println!("page_move_refused direction={dir_s} detail={e}");
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    let chrome = chrome_for_page(manifest, &page_id)?;
    let label = chrome.label();
    let strokes = ctrl.session.committed_strokes().to_vec();
    if let Err(e) = ctrl
        .transport_mut()
        .reconstruct_page(current_bg, &strokes, Some(chrome))
    {
        return Err(format!(
            "FATAL: page move durable but reconstruct failed: {e}"
        ));
    }
    ctrl.clear_pending_clean();
    println!(
        "page_moved page={} direction={dir_s} from_index={} to_index={} hud=\"{label}\" manifest_persist_us={} page_count={}",
        commit.page_id,
        commit.from_index,
        commit.to_index,
        commit.manifest_persist_us,
        manifest.page_ids.len()
    );
    Ok(true)
}

/// F9: durable insert blank page after current. Binds/reconstructs the new page.
fn insert_page_after_current(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    ctrl: &mut KindleCtrl,
    notebook_profile: &RenderProfile,
    current_bg: &mut Vec<u8>,
) -> Result<bool, String> {
    if !ctrl.is_idle() || ctrl.life().is_armed() {
        println!("page_insert_ignored reason=not_idle");
        return Ok(false);
    }
    let after_id = ctrl.session.page_id().to_string();
    let commit =
        match insert_page_after_durable(data_dir, manifest, &after_id, &InsertPageHooks::default())
        {
            Ok(c) => c,
            Err(e) if e.starts_with("REFUSE PAGE INSERT") => {
                println!("page_insert_refused detail={e}");
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
    crate::sync_outbound::with_global(|s| {
        s.page_created(
            data_dir,
            &manifest.notebook_id,
            &commit.inserted_page_id,
            Some(&after_id),
        )
    });
    println!(
        "page_inserted after={} new={} index={} manifest_persist_us={} page_count={}",
        after_id,
        commit.inserted_page_id,
        commit.insert_index,
        commit.manifest_persist_us,
        manifest.page_ids.len()
    );

    let geo = ctrl.transport().geometry();
    let (new_store, loaded) =
        match load_page_for_switch(data_dir, &commit.inserted_page_id, notebook_profile, &geo) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "FATAL: page insert durable but new page load failed: {e}"
                ));
            }
        };
    let chrome = chrome_for_page(manifest, &commit.inserted_page_id)?;
    let label = chrome.label();
    let strokes = loaded.strokes.clone();
    let roots = loaded.root_of.clone();
    let bg = loaded.background.clone();
    let (texts, links) =
        load_and_paint_page_content(ctrl.transport_mut(), data_dir, &commit.inserted_page_id)
            .map_err(|e| format!("FATAL: page insert durable but new page texts failed: {e}"))?;
    if let Err(e) = ctrl
        .transport_mut()
        .reconstruct_page(&bg, &strokes, Some(chrome))
    {
        return Err(format!(
            "FATAL: page insert durable but reconstruct failed: {e}"
        ));
    }
    if let Err(e) = ctrl.bind_page_with_roots(
        BoundPageKey::new(&manifest.notebook_id, &commit.inserted_page_id),
        strokes,
        roots,
        RuntimeStore::Bound(new_store),
        texts,
        links,
    ) {
        return Err(format!("FATAL: page insert durable but bind failed: {e}"));
    }
    *current_bg = bg;
    ctrl.clear_pending_clean();
    println!(
        "page_insert_active={} hud=\"{}\" strokes={}",
        commit.inserted_page_id,
        label,
        ctrl.session.committed_count()
    );
    Ok(true)
}

/// S2B: settle a write-ahead sync record after the local mutation's outcome is known.
fn sync_resolve(ticket: Option<crate::sync_outbound::PrepTicket>, committed: bool) {
    if let Some(t) = ticket {
        crate::sync_outbound::with_global(|s| if committed { s.commit(t) } else { s.abort(t) });
    }
}

/// Write-ahead ticket when outbound sync is installed. `Ok(None)` = sync is off (local
/// mutation proceeds untracked). `Err` = sync is on but the required capture intent is not
/// durable — the caller must refuse the mutation.
fn sync_prepare<T>(
    f: impl FnOnce(&crate::sync_outbound::OutboundSync) -> Option<T>,
) -> Result<Option<T>, String> {
    match crate::sync_outbound::with_global(f) {
        None => Ok(None),
        Some(None) => Err("local capture journal could not record this edit".into()),
        Some(Some(t)) => Ok(Some(t)),
    }
}

/// S2B: capture a durably created notebook (appended after the previous library entry).
fn sync_notebook_created(library: &LibraryRepository, notebook_id: &str, title: &str) {
    let ids = &library.manifest().notebook_ids;
    let after = ids
        .iter()
        .position(|n| n == notebook_id)
        .and_then(|i| i.checked_sub(1))
        .map(|i| ids[i].as_str());
    crate::sync_outbound::with_global(|s| {
        s.notebook_created(
            &library.notebook_dir(notebook_id),
            notebook_id,
            title,
            after,
        )
    });
}

/// S2B: one eraser gesture's sync capture. When outbound sync is installed, local fragment IDs
/// are the deterministic `d-<UUIDv5>` of the erase operation (so local and server state
/// converge) even if upload is faulted or the server is unreachable. Without an installed
/// OutboundSync the gesture uses random fragment IDs (developer runs with no `--sync-dir`).
struct SyncedErase {
    ctx: Option<(sync_protocol::OperationId, String, String)>,
    intent: Option<crate::eraser::EraseIntent>,
}

impl SyncedErase {
    fn begin(eraser: &EraserGestureState, notebook: &str, page: &str) -> Self {
        let ctx = crate::sync_outbound::with_global(|s| {
            (s.erase_op_id(), notebook.to_string(), page.to_string())
        });
        Self {
            intent: ctx.as_ref().and_then(|_| eraser.intent()),
            ctx,
        }
    }

    fn on_up(&self, eraser: &mut EraserGestureState) -> crate::eraser::PartialEraseCommit {
        let Some((op, nb, pg)) = &self.ctx else {
            return eraser.on_up();
        };
        let mut failed = None;
        let commit = eraser.on_up_with(&mut |src, i| {
            let id = crate::sync_outbound::with_global(|s| {
                s.erase_fragment_local_id(*op, nb, pg, src, i)
            });
            match id {
                Some(Ok(id)) => id,
                other => {
                    if let Some(Err(e)) = other {
                        failed.get_or_insert(e);
                    }
                    crate::eraser::new_fragment_local_id()
                }
            }
        });
        if let Some(e) = failed {
            crate::sync_outbound::with_global(|s| s.fail_external(format!("erase ids: {e}")));
        }
        commit
    }

    /// Write-ahead `EraseRegion` for a dirty gesture (before the durable replace).
    /// `Ok(None)` = nothing to capture (sync off, or a clean gesture). `Err` = the required
    /// capture intent is not durable; the caller must not persist the replace.
    fn prepare(
        &self,
        commit: &crate::eraser::PartialEraseCommit,
    ) -> Result<Option<crate::sync_outbound::PrepTicket>, String> {
        let Some((op, nb, pg)) = &self.ctx else {
            return Ok(None);
        };
        if !commit.dirty {
            return Ok(None);
        }
        let Some(intent) = &self.intent else {
            crate::sync_outbound::with_global(|s| {
                s.fail_external("erase gesture has no canonical sweep path".into())
            });
            return Err("erase gesture has no canonical sweep path".into());
        };
        match crate::sync_outbound::with_global(|s| {
            s.prepare_erase(*op, nb, pg, &intent.observed_roots, &intent.path)
        }) {
            Some(Some(t)) => Ok(Some(t)),
            Some(None) | None => Err("local capture journal could not record this erase".into()),
        }
    }
}

fn finger_page_point(ctrl: &KindleCtrl, point: UiPoint) -> Option<Point> {
    let page = ctrl
        .transport()
        .profile
        .transform()
        .ok()?
        .viewport()
        .to_page(DisplayPoint::new(point.x as f32, point.y as f32))?;
    Some(Point::new(page.x, page.y))
}

fn selected_visible_ids(ctrl: &KindleCtrl, selection: &LassoSelection) -> HashSet<String> {
    let roots: HashSet<&str> = selection.roots().collect();
    ctrl.session
        .committed_strokes()
        .iter()
        .filter(|stroke| roots.contains(ctrl.session.root_id_of(&stroke.id).as_str()))
        .map(|stroke| stroke.id.as_str().to_string())
        .collect()
}

fn current_selection_panel_bounds(ctrl: &KindleCtrl, lasso: &LassoState) -> Option<Rect> {
    ctrl.transport()
        .canonical_rect_panel(lasso.selection()?.bounds)
}

fn clear_lasso_overlay(
    ctrl: &mut KindleCtrl,
    lasso: &mut LassoState,
    overlay: &mut TouchOverlayDirty,
    cached_move_ids: &mut Option<HashSet<String>>,
) -> Result<(), String> {
    lasso.clear();
    cached_move_ids.take();
    ctrl.transport_mut().reset_lasso_dash();
    let dirty = overlay.take_cleanup();
    let (transport, strokes) = ctrl.transport_and_strokes();
    transport.clear_touch_overlay(strokes, dirty)
}

/// Live preview of the latest selection-move position. Intermediate points are not required.
fn present_selection_move_preview(
    ctrl: &mut KindleCtrl,
    lasso: &mut LassoState,
    overlay: &mut TouchOverlayDirty,
    cached_move_ids: &mut Option<HashSet<String>>,
    point: UiPoint,
) -> Result<(), String> {
    let Some(page) = finger_page_point(ctrl, point) else {
        return Ok(());
    };
    let Some((dx, dy)) = lasso.update_move(page) else {
        return Ok(());
    };
    let Some(selection) = lasso.selection().cloned() else {
        return Ok(());
    };
    let ids = cached_move_ids
        .get_or_insert_with(|| selected_visible_ids(ctrl, &selection))
        .clone();
    let restore = overlay.take_cleanup();
    let (transport, strokes) = ctrl.transport_and_strokes();
    overlay.move_preview = Some(transport.present_touch_selection(
        strokes,
        restore,
        selection.bounds,
        &ids,
        dx,
        dy,
        false,
        "move_point",
    )?);
    Ok(())
}

/// One local atomic edit plus one write-ahead sync operation. Preview frames never call this.
fn commit_lasso_move(
    ctrl: &mut KindleCtrl,
    manifest: &PersistedNotebookManifestV1,
    lasso: &LassoState,
    movement: &MoveCommit,
    overlay: &mut TouchOverlayDirty,
) -> Result<(), String> {
    if movement.bound != *ctrl.open_page().bound() {
        return Err("lasso move belongs to a different bound page".into());
    }
    let replacements =
        move_replacements(&ctrl.open_page(), &movement.roots, movement.dx, movement.dy)?;
    let ticket = sync_prepare(|sync| {
        sync.prepare_move_stroke_lineages(
            &manifest.notebook_id,
            ctrl.session.page_id(),
            &movement.roots,
            movement.dx,
            movement.dy,
        )
    })?;
    let timing = match ctrl.durable_replace_strokes(&replacements) {
        Ok(timing) => {
            sync_resolve(ticket, true);
            timing
        }
        Err(error) => {
            sync_resolve(ticket, false);
            return Err(format!("lasso move persist failed: {error}"));
        }
    };
    let selection = lasso
        .selection()
        .ok_or("lasso selection disappeared after move")?;
    let move_dirty = ctrl
        .transport()
        .canonical_rect_dirty(
            move_dirty_bounds(movement.old_bounds, movement.new_bounds),
            12,
        )
        .ok_or("lasso move dirty bounds outside display transform")?;
    let restore = overlay
        .take_cleanup()
        .map_or(move_dirty, |preview| preview.union(move_dirty));
    let empty = HashSet::new();
    let (transport, strokes) = ctrl.transport_and_strokes();
    overlay.move_preview = Some(transport.present_touch_selection(
        strokes,
        Some(restore),
        selection.bounds,
        &empty,
        0.0,
        0.0,
        true,
        "move_commit",
    )?);
    println!(
        "lasso_move lineages={} fragments={} dx={} dy={} persist_append_us={} persist_fsync_us={} regional_gc16=true",
        movement.roots.len(),
        replacements.len(),
        movement.dx,
        movement.dy,
        timing.append_us,
        timing.fsync_us,
    );
    Ok(())
}

/// TEMP DIAG: per-frame touch latency sample from the pump drain loop.
#[derive(Clone, Copy, Debug, Default)]
struct TouchFrameDiag {
    event_age_ms: u64,
    backlog_after: usize,
    produced_seq: u64,
    consumed_seq: u64,
}

/// Handle touch-manipulation actions. `Ok(false)` means the legacy tap/swipe path should run.
/// `SelectionMovePoint` updates `LassoState` only; the drain loop paints latest-state previews.
#[allow(clippy::too_many_arguments)]
fn handle_lasso_action(
    action: FingerAction,
    ctrl: &mut KindleCtrl,
    manifest: &PersistedNotebookManifestV1,
    lasso: &mut LassoState,
    overlay: &mut TouchOverlayDirty,
    last_move_undo: &mut Option<MoveCommit>,
    cached_move_ids: &mut Option<HashSet<String>>,
    frame_diag: Option<TouchFrameDiag>,
) -> Result<bool, String> {
    match action {
        FingerAction::Tap(_) | FingerAction::NextPage | FingerAction::PreviousPage => {
            return Ok(false)
        }
        FingerAction::SelectionCleared => {
            clear_lasso_overlay(ctrl, lasso, overlay, cached_move_ids)?;
        }
        FingerAction::LassoStarted(point) => {
            if !ctrl.is_idle() || ctrl.is_armed() {
                return Ok(true);
            }
            let Some(page) = finger_page_point(ctrl, point) else {
                return Ok(true);
            };
            let bound = ctrl.open_page().bound().clone();
            lasso.begin_lasso(bound, page);
            ctrl.transport_mut().reset_lasso_dash();
            overlay.clear();
            cached_move_ids.take();
        }
        FingerAction::LassoPoint(point) => {
            let Some(page) = finger_page_point(ctrl, point) else {
                return Ok(true);
            };
            let from = lasso.drawing_points().and_then(|p| p.last()).copied();
            if lasso.push_lasso_point(page) {
                if let Some(from) = from {
                    if let Some(dirty) = ctrl.transport_mut().draw_lasso_segment(from, page)? {
                        overlay.add_lasso_segment(dirty);
                    }
                }
            }
        }
        FingerAction::LassoFinished(point) => {
            if let Some(page) = finger_page_point(ctrl, point) {
                let from = lasso.drawing_points().and_then(|p| p.last()).copied();
                if lasso.push_lasso_point(page) {
                    if let Some(from) = from {
                        if let Some(dirty) = ctrl.transport_mut().draw_lasso_segment(from, page)? {
                            overlay.add_lasso_segment(dirty);
                        }
                    }
                }
            }
            let selected = {
                let open = ctrl.open_page();
                lasso.finish_lasso(&open).cloned()
            };
            match selected {
                None => {
                    clear_lasso_overlay(ctrl, lasso, overlay, cached_move_ids)?;
                    println!("lasso_selection=empty");
                }
                Some(selection) => {
                    let ids = selected_visible_ids(ctrl, &selection);
                    *cached_move_ids = Some(ids.clone());
                    let restore = overlay.take_cleanup();
                    let (transport, strokes) = ctrl.transport_and_strokes();
                    overlay.move_preview = Some(transport.present_touch_selection(
                        strokes,
                        restore,
                        selection.bounds,
                        &ids,
                        0.0,
                        0.0,
                        true,
                        "lasso_finished",
                    )?);
                    println!(
                        "lasso_selection lineages={} regional_gc16=true",
                        selection.objects.len()
                    );
                }
            }
        }
        FingerAction::LassoCancelled => {
            clear_lasso_overlay(ctrl, lasso, overlay, cached_move_ids)?;
        }
        FingerAction::SelectionMoveStarted(point) => {
            if let Some(diag) = frame_diag {
                println!(
                    "lasso_move_input kind=started event_age_ms={} backlog_after={} produced_seq={} consumed_seq={} x={} y={}",
                    diag.event_age_ms,
                    diag.backlog_after,
                    diag.produced_seq,
                    diag.consumed_seq,
                    point.x,
                    point.y,
                );
            }
            if let Some(page) = finger_page_point(ctrl, point) {
                let _ = lasso.begin_move(page);
            }
            if let Some(selection) = lasso.selection() {
                *cached_move_ids = Some(selected_visible_ids(ctrl, selection));
            }
        }
        FingerAction::SelectionMovePoint(point) => {
            // State only. Preview paint is latest-state and happens in the drain loop.
            if let Some(page) = finger_page_point(ctrl, point) {
                let _ = lasso.update_move(page);
            }
        }
        FingerAction::SelectionMoveFinished(point) => {
            if let Some(diag) = frame_diag {
                println!(
                    "lasso_move_input kind=finished event_age_ms={} backlog_after={} produced_seq={} consumed_seq={} x={} y={}",
                    diag.event_age_ms,
                    diag.backlog_after,
                    diag.produced_seq,
                    diag.consumed_seq,
                    point.x,
                    point.y,
                );
            }
            if let Some(page) = finger_page_point(ctrl, point) {
                let _ = lasso.update_move(page);
            }
            if let Some(movement) = lasso.finish_move() {
                commit_lasso_move(ctrl, manifest, lasso, &movement, overlay)?;
                *last_move_undo = Some(MoveCommit {
                    bound: movement.bound,
                    roots: movement.roots,
                    dx: -movement.dx,
                    dy: -movement.dy,
                    old_bounds: movement.new_bounds,
                    new_bounds: movement.old_bounds,
                });
            } else if let Some(selection) = lasso.selection().cloned() {
                let ids = cached_move_ids
                    .get_or_insert_with(|| selected_visible_ids(ctrl, &selection))
                    .clone();
                let restore = overlay.take_cleanup();
                let (transport, strokes) = ctrl.transport_and_strokes();
                overlay.move_preview = Some(transport.present_touch_selection(
                    strokes,
                    restore,
                    selection.bounds,
                    &ids,
                    0.0,
                    0.0,
                    true,
                    "move_finished_noop",
                )?);
            }
        }
        FingerAction::SelectionMoveCancelled => {
            lasso.cancel_move();
            if let Some(selection) = lasso.selection().cloned() {
                let ids = cached_move_ids
                    .get_or_insert_with(|| selected_visible_ids(ctrl, &selection))
                    .clone();
                let restore = overlay.take_cleanup();
                let (transport, strokes) = ctrl.transport_and_strokes();
                overlay.move_preview = Some(transport.present_touch_selection(
                    strokes,
                    restore,
                    selection.bounds,
                    &ids,
                    0.0,
                    0.0,
                    true,
                    "move_cancelled",
                )?);
            } else {
                clear_lasso_overlay(ctrl, lasso, overlay, cached_move_ids)?;
            }
        }
    }
    Ok(true)
}

/// F4/F7 undo of the newest visible stroke lineage + reconstruct (shared by the sealed
/// side+center gesture and the F11 UNDO button). Every error is fatal (durable edit kept).
fn undo_and_present(
    ctrl: &mut KindleCtrl,
    manifest: &PersistedNotebookManifestV1,
    current_bg: &[u8],
    lasso: &mut LassoState,
    overlay: &mut TouchOverlayDirty,
    last_move_undo: &mut Option<MoveCommit>,
    cached_move_ids: &mut Option<HashSet<String>>,
) -> Result<(), String> {
    if let Some(inverse) = last_move_undo.take() {
        lasso.select_roots(
            inverse.bound.clone(),
            inverse.roots.clone(),
            inverse.new_bounds,
        );
        commit_lasso_move(ctrl, manifest, lasso, &inverse, overlay)?;
        println!(
            "undo_move lineages={} dx={} dy={} redo_available=false",
            inverse.roots.len(),
            inverse.dx,
            inverse.dy
        );
        return Ok(());
    }
    // The established lineage-delete undo can invalidate a selected root. Its full durable
    // reconstruct below also removes the transient box, so drop the local selection now.
    lasso.clear();
    overlay.clear();
    cached_move_ids.take();
    let undo_root = ctrl
        .session
        .last_committed_stroke_id()
        .map(|id| ctrl.session.root_id_of(id));
    match ctrl.undo_last_visible_stroke() {
        Ok(UndoResult::Empty) => {
            println!("undo_result=empty");
            Ok(())
        }
        Ok(UndoResult::Deleted { stroke_id, timing }) => {
            if let Some(root) = &undo_root {
                crate::sync_outbound::with_global(|s| {
                    s.lineage_deleted(&manifest.notebook_id, ctrl.session.page_id(), root)
                });
            }
            let remaining: Vec<Stroke> = ctrl.session.committed_strokes().to_vec();
            let chrome = chrome_for_page(manifest, ctrl.session.page_id())
                .map_err(|e| format!("undo fatal=true label: {e}"))?;
            match ctrl
                .transport_mut()
                .reconstruct_page(current_bg, &remaining, Some(chrome))
            {
                Ok((reconstruct_us, gc16_us)) => {
                    ctrl.clear_pending_clean();
                    println!(
                        "undo stroke_id={stroke_id} undo_durable=true \
                         persist_append_us={} persist_fsync_us={} \
                         persist_commit_us={} reconstruct_us={reconstruct_us} \
                         gc16_us={gc16_us}",
                        timing.append_us,
                        timing.fsync_us,
                        timing.commit_us(),
                    );
                    Ok(())
                }
                Err(e) => {
                    ctrl.totals.undo_reconstruct_failures += 1;
                    Err(format!(
                        "undo stroke_id={stroke_id} undo_durable=true \
                         panel_reconstruct=false fatal=true: {e}"
                    ))
                }
            }
        }
        Err(e) => Err(format!("error: undo: {e}")),
    }
}

/// Mutable runtime state a UI command may act on. The durable operations themselves are the
/// existing F8–F10 primitives; this layer only sequences them and presents the result.
struct UiExec<'a> {
    ink_request: InkRequest,
    geo: FbGeometry,
    library: &'a mut LibraryRepository,
    notebook_dir: &'a mut PathBuf,
    manifest: &'a mut PersistedNotebookManifestV1,
    notebook_profile: &'a mut RenderProfile,
    current_bg: &'a mut Vec<u8>,
    ui: &'a mut UiState,
    /// Kindle-local folders (`<library>/.scribe-folders`; never synced).
    folders: &'a mut FolderStore,
    /// Launched by `scribe-lifecycle` under Kindle appmgrd (`--display-events-stdin`).
    hosted: bool,
    stop: &'a Arc<AtomicBool>,
    /// NBK import in progress (Tools screen); one source per idle loop pass.
    import_run: &'a mut Option<ImportRun>,
    /// Replica-local preferences.json (outside the library).
    prefs_path: &'a Path,
    /// `--sync-dir` (Restore from Server replaces it with the library).
    sync_dir: Option<&'a Path>,
    lasso: &'a mut LassoState,
    overlay: &'a mut TouchOverlayDirty,
    cached_move_ids: &'a mut Option<HashSet<String>>,
    last_move_undo: &'a mut Option<MoveCommit>,
}

/// One "Import all" run: the eligible sources still to import plus running totals.
struct ImportRun {
    queue: std::collections::VecDeque<crate::nbk_import::SourceNotebook>,
    total: usize,
    imported: usize,
    skipped: usize,
    failures: Vec<crate::tools_ui::ImportFailureLine>,
}

fn tools_scan_phase(
    library: &LibraryRepository,
) -> (
    crate::tools_ui::ToolsPhase,
    Option<crate::nbk_import::ScanReport>,
) {
    use crate::tools_ui::ToolsPhase;
    match crate::nbk_import::scan(&crate::nbk_import::ImportSources::kindle(), library.root()) {
        Ok(r) => {
            println!(
                "nbk_import_scan total={} eligible={} already_imported={}",
                r.total(),
                r.eligible().len(),
                r.already_imported()
            );
            (
                ToolsPhase::Scanned {
                    total: r.total(),
                    eligible: r.eligible().len(),
                    already_imported: r.already_imported(),
                },
                Some(r),
            )
        }
        Err(e) => {
            eprintln!("nbk_import_scan_failed detail={e}");
            (
                ToolsPhase::Error("The Kindle notebook list could not be read.".into()),
                None,
            )
        }
    }
}

/// Import the next queued Amazon notebook, then repaint progress (or the final summary).
/// Runs only at an idle safe point in the Tools view; never on the pen path.
fn import_step(x: &mut UiExec<'_>, ctrl: &mut KindleCtrl) -> Result<(), String> {
    use crate::nbk_import::{import_one, GlobalSync, ImportOutcome, ImportTarget, Ledger};
    use crate::tools_ui::{failure_reason, ImportFailureLine, ToolsPhase};
    let Some(run) = x.import_run.as_mut() else {
        return Ok(());
    };
    if let Some(src) = run.queue.pop_front() {
        let paper = paper_background(&x.geo);
        let t0 = Instant::now();
        let outcome = match Ledger::open(x.library.root()) {
            Ok(mut ledger) => {
                let mut target = ImportTarget {
                    library: &mut *x.library,
                    profile: &*x.notebook_profile,
                    geo: &x.geo,
                    paper: &paper,
                    cancel: x.stop,
                };
                import_one(&mut target, &mut ledger, &src, &GlobalSync)
            }
            Err(e) => ImportOutcome::Failed(e),
        };
        let ms = t0.elapsed().as_millis();
        match &outcome {
            ImportOutcome::Imported {
                notebook_id,
                pages,
                strokes,
                skipped_empty_strokes,
            } => {
                run.imported += 1;
                println!(
                    "nbk_import_done origin={} notebook={notebook_id} pages={pages} strokes={strokes} skipped_empty_strokes={skipped_empty_strokes} ms={ms}",
                    src.origin_id
                );
            }
            ImportOutcome::AlreadyImported => {
                run.skipped += 1;
                println!(
                    "nbk_import_skipped origin={} reason=already_imported",
                    src.origin_id
                );
            }
            ImportOutcome::Failed(e) => {
                eprintln!(
                    "nbk_import_failed origin={} ms={ms} detail={e}",
                    src.origin_id
                );
                run.failures.push(ImportFailureLine {
                    title: src.title.clone(),
                    reason: failure_reason(e),
                });
            }
            // Sync stopped (or shutdown): stop the whole run. No later notebook is created
            // locally without durable sync history; the rest stay eligible for a later run.
            ImportOutcome::Halted(e) => {
                eprintln!(
                    "nbk_import_halted origin={} ms={ms} not_started={} detail={e}",
                    src.origin_id,
                    run.queue.len()
                );
                run.failures.push(ImportFailureLine {
                    title: src.title.clone(),
                    reason: failure_reason(e),
                });
                run.queue.clear();
            }
        }
    }
    let done = run.imported + run.skipped + run.failures.len();
    x.ui.tools.phase = match run.queue.front() {
        Some(next) => ToolsPhase::Importing {
            done,
            total: run.total,
            current: next.title.clone(),
        },
        None => ToolsPhase::Finished {
            imported: run.imported,
            skipped: run.skipped,
            failed: run.failures.len(),
            failures: run.failures.clone(),
        },
    };
    if run.queue.is_empty() {
        println!(
            "nbk_import_finished imported={} skipped={} failed={}",
            run.imported,
            run.skipped,
            run.failures.len()
        );
        *x.import_run = None;
    }
    present_ui(x, ctrl, None)
}

/// Kindle home (Library) app. appmgrd switching to it pauses/unloads `com.markrai.ignite`;
/// the lifecycle host then SIGTERMs this process, which shuts down through the normal
/// signal path (disarm, EVIOCGRAB release, `shutdown_safe`). Observed on FW 5.18.6:
/// `pause` → `child_stop signal=SIGTERM` → `child_exited status=0` → `unload`.
const KINDLE_HOME_APP_URI: &str = "app://com.lab126.KPPMainApp";

/// Home exit icon. Hosted: ask appmgrd to leave Scribe (never exit behind appmgrd's back, which
/// would leave the Kindle foregrounding a dead app). Standalone (SSH/physical tests): request
/// the same clean shutdown as Ctrl+C.
fn request_app_exit(x: &mut UiExec<'_>, ctrl: &mut KindleCtrl) -> Result<(), String> {
    if !x.hosted {
        println!("app_exit requested=true mode=standalone");
        x.stop.store(true, Ordering::SeqCst);
        return Ok(());
    }
    match std::process::Command::new("lipc-set-prop")
        .args(["com.lab126.appmgrd", "start", KINDLE_HOME_APP_URI])
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            println!("app_exit requested=true mode=appmgrd target={KINDLE_HOME_APP_URI}");
            // appmgrd will SIGTERM us through the lifecycle host; never block the UI loop on it.
            thread::spawn(move || match child.wait() {
                Ok(st) => println!("app_exit lipc_set_prop status={st}"),
                Err(e) => eprintln!("app_exit lipc_set_prop wait failed: {e}"),
            });
            Ok(())
        }
        Err(e) => {
            eprintln!("app_exit_failed detail={e}");
            x.ui.show_error("Ignite couldn't exit.");
            present_ui(x, ctrl, None)
        }
    }
}

fn active_notebook_index(library: &LibraryRepository) -> usize {
    HomeState::for_active(library).selected_index
}

/// Refresh the UI's folder snapshot (live members only) from the folder store.
fn refresh_folder_view(ui: &mut UiState, folders: &FolderStore, library: &LibraryRepository) {
    ui.folders = folders.view(&library.manifest().notebook_ids);
    ui.next_folder_title = folders.next_default_title();
}

/// Run one folder-store mutation from the UI; failures become an error modal.
/// A committed local folder change to capture for sync (post-commit).
enum FolderCapture {
    Created(String),
    Renamed(String),
    /// Global identity resolved before the local delete.
    Deleted(sync_protocol::FolderId),
    Filed {
        notebook: String,
        folder: Option<String>,
    },
    /// Sync is off: nothing to capture.
    Nothing,
}

/// Capture a committed folder change (no-op while sync is off or faulted). Global folder IDs
/// come from the folder store; the notebook's from the sync identity map.
fn capture_folder_change(folders: &FolderStore, change: FolderCapture) {
    crate::sync_outbound::with_global(|s| {
        let library = s.library_id();
        let gid = |local: &str| folders.sync_id(local, library);
        let op = |c: &FolderCapture| -> Option<sync_protocol::SyncOperationV1> {
            use sync_protocol::SyncOperationV1 as O;
            let view = |id: &str| folders.title(id).map(str::to_string);
            Some(match c {
                FolderCapture::Created(id) => O::CreateFolder {
                    folder_id: gid(id)?,
                    title: view(id)?,
                },
                FolderCapture::Renamed(id) => O::RenameFolder {
                    folder_id: gid(id)?,
                    title: view(id)?,
                },
                FolderCapture::Deleted(g) => O::DeleteFolder { folder_id: *g },
                FolderCapture::Filed { .. } | FolderCapture::Nothing => return None,
            })
        };
        match &change {
            FolderCapture::Nothing => {}
            FolderCapture::Filed { notebook, folder } => match folder.as_deref().map(gid) {
                Some(None) => s.fail_external(format!("folder {folder:?} has no identity")),
                resolved => s.notebook_folder_set(notebook, resolved.flatten()),
            },
            c => match op(c) {
                Some(o) => s.folder_changed(o),
                None => s.fail_external("folder capture: folder not found after commit".into()),
            },
        }
    });
}

fn folder_mutation(
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
    what: &str,
    failure: &str,
    op: impl FnOnce(&mut FolderStore, &[String]) -> Result<FolderCapture, String>,
) -> Result<(), String> {
    let live = x.library.manifest().notebook_ids.clone();
    match op(x.folders, &live) {
        Ok(change) => {
            capture_folder_change(x.folders, change);
            println!(
                "folder_{what} ok=true folders={}",
                x.folders.view(&live).len()
            );
        }
        Err(e) => {
            eprintln!("folder_{what}_failed detail={e}");
            x.ui.show_error(failure);
        }
    }
    present_ui(x, ctrl, None)
}

/// 1H-D2: set when a library restore needs the page session restarted (the binary's session
/// loop runs it again in the same process; startup recovery then completes the restore).
static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Consume a pending session-restart request.
pub fn take_restart_request() -> bool {
    RESTART_REQUESTED.swap(false, Ordering::SeqCst)
}

/// A Restore-from-Server flow is on screen: no developer library operations meanwhile.
fn restore_flow_open(ui: &UiState) -> bool {
    ui.view == AppView::Sync && ui.sync_view.as_ref().is_some_and(|v| v.restore.is_some())
}

/// Repaint whatever the UI state says is on screen. GC16 FULL unless `region` (keyboard).
fn present_ui(
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
    region: Option<Rect>,
) -> Result<(), String> {
    if let Some(modal) = x.ui.modal.as_ref().filter(|m| m.is_title_entry()) {
        return ctrl.transport_mut().render_title_entry(modal, region);
    }
    match x.ui.view {
        AppView::Home => {
            if ctrl.store().is_bound() && !x.manifest.notebook_id.is_empty() {
                x.library
                    .set_cached_page_count(&x.manifest.notebook_id, x.manifest.page_ids.len());
            }
            let cards = home_cards(x.library);
            refresh_folder_view(x.ui, x.folders, x.library);
            x.ui.settle_home(&cards);
            let open = x.ui.open_folder().map(|f| f.id.clone());
            ctrl.transport_mut().render_home(
                &cards,
                &x.ui.folders,
                open.as_deref(),
                &x.ui.home,
                x.ui.modal.as_ref(),
            )
        }
        AppView::Sync => {
            let Some(view) = crate::server_link::with_global(|l| l.view()) else {
                return Err("internal: Sync view without a server link".into());
            };
            x.ui.sync_view = Some(view.clone());
            ctrl.transport_mut().render_sync(&view)
        }
        AppView::Tools => ctrl.transport_mut().render_tools(&x.ui.tools),
        AppView::Preferences => {
            let dialog = x.ui.modal.as_ref().and_then(dialog_spec);
            ctrl.transport_mut()
                .render_preferences(x.ui.side_button, dialog.as_ref())
        }
        AppView::LinkTargetPicker => {
            let dialog = x.ui.modal.as_ref().and_then(dialog_spec);
            let cards = home_cards(x.library);
            let Some(picker) = x.ui.link_picker.as_mut() else {
                return Err("internal: LinkTargetPicker without picker state".into());
            };
            fill_picker_thumbs(x.library, picker, &x.geo);
            ctrl.transport_mut()
                .render_link_picker(picker, &cards, dialog.as_ref())
        }
        AppView::Notebook => {
            let page_id = ctrl.session.page_id().to_string();
            let chrome = chrome_for_page(x.manifest, &page_id)?;
            let strokes = ctrl.session.committed_strokes().to_vec();
            let (texts, links) =
                load_and_paint_page_content(ctrl.transport_mut(), x.notebook_dir, &page_id)
                    .map_err(|e| format!("load page texts: {e}"))?;
            ctrl.replace_texts(texts);
            ctrl.replace_links(links);
            ctrl.transport_mut()
                .set_chrome_visible(x.ui.chrome_overlay_visible());
            ctrl.transport_mut().present_notebook(
                x.current_bg,
                &strokes,
                chrome,
                x.ui.modal.as_ref(),
            )?;
            ctrl.clear_pending_clean();
            Ok(())
        }
    }
}

/// S2C: apply staged inbound server operations at an idle safe point (no gesture, no modal,
/// display valid), then reload the durable library/notebook/page and present ONE coherent
/// frame. Materialization never authors local sync operations (RemoteMaterialization origin).
/// Sync problems fault sync only; the notebook keeps working. `Ok(true)` = something ran.
fn apply_inbound_at_safe_point(x: &mut UiExec<'_>, ctrl: &mut KindleCtrl) -> Result<bool, String> {
    let Some(result) =
        crate::sync_outbound::with_global(|s| s.inbound_ready().then(|| s.apply_inbound()))
            .flatten()
    else {
        return Ok(false);
    };
    let applied = match result {
        Ok(Some(a)) => a,
        Ok(None) => return Ok(false),
        Err(e) => {
            eprintln!("sync_inbound_fault detail={e}");
            return Ok(false);
        }
    };
    println!(
        "sync_inbound_applied from={} through={} ops={} own_echoes={} changed={} report={:?}",
        applied.from_cursor,
        applied.through,
        applied.ops,
        applied.own_echoes,
        applied.report.changed(),
        applied.report
    );
    if !applied.report.changed() {
        return Ok(true);
    }
    reload_after_inbound(x, ctrl)
        .map_err(|e| format!("FATAL: inbound materialized but reload failed: {e}"))?;
    Ok(true)
}

/// Rebind the runtime to the (remotely changed) durable state. Replica-local UI state (view,
/// selection, chrome, modal) is kept; the active notebook/page are the durable ones, which the
/// materializer only changes when the selected entity was deleted.
fn reload_after_inbound(x: &mut UiExec<'_>, ctrl: &mut KindleCtrl) -> Result<(), String> {
    let root = x.library.root().to_path_buf();
    *x.library = crate::library::open_library_read_only(&root)?;
    // Remote folder changes were written to folders.json: never keep a stale in-memory copy
    // (a later local change would overwrite them).
    x.folders.reload();
    if x.ui.view != AppView::Notebook && !ctrl.store().is_bound() {
        present_ui(x, ctrl, None)?;
        println!(
            "sync_inbound_presented view={:?} notebook=unbound",
            x.ui.view
        );
        return Ok(());
    }
    let paper = paper_background(&x.geo);
    let (new_dir, opened) = open_active_notebook(x.library, x.ink_request, &x.geo, &paper)?;
    let active = x.library.active_notebook_id().to_string();
    let title = x.library.title(&active)?.to_string();
    let page_id = opened.page_id.clone();
    let profile = opened.loaded.render_profile.clone();
    ctrl.transport_mut().set_profile(profile.clone());
    ctrl.transport_mut().set_chrome_title(&title);
    // Overlay + retained texts from the same load: do not bind texts while leaving a stale overlay.
    let (texts, links) = load_and_paint_page_content(ctrl.transport_mut(), &new_dir, &page_id)
        .map_err(|e| format!("load page texts: {e}"))?;
    ctrl.bind_page_with_roots(
        BoundPageKey::new(&opened.manifest.notebook_id, &page_id),
        opened.loaded.strokes.clone(),
        opened.loaded.root_of.clone(),
        RuntimeStore::Bound(opened.store),
        texts,
        links,
    )
    .map_err(|e| format!("bind page: {e}"))?;
    *x.notebook_dir = new_dir;
    *x.manifest = opened.manifest;
    x.library
        .set_cached_page_count(&active, x.manifest.page_ids.len());
    *x.notebook_profile = profile;
    *x.current_bg = opened.loaded.background;
    ctrl.clear_pending_clean();
    present_ui(x, ctrl, None)?;
    println!(
        "sync_inbound_presented view={:?} notebook={active} page={page_id} strokes={}",
        x.ui.view,
        ctrl.session.committed_count()
    );
    Ok(())
}

/// Open + bind the library's (already committed) active notebook. Post-commit: fatal on error.
fn install_active_notebook(x: &mut UiExec<'_>, ctrl: &mut KindleCtrl) -> Result<(), String> {
    let paper = paper_background(&x.geo);
    let (new_dir, opened) = open_active_notebook(x.library, x.ink_request, &x.geo, &paper)
        .map_err(|e| format!("FATAL: notebook switch committed but target load failed: {e}"))?;
    let active = x.library.active_notebook_id().to_string();
    let title = x.library.title(&active)?.to_string();
    install_opened_notebook(
        &active,
        &title,
        new_dir,
        opened,
        ctrl,
        ActiveNotebookBindings {
            notebook_dir: x.notebook_dir,
            manifest: x.manifest,
            profile: x.notebook_profile,
            background: x.current_bg,
        },
    )?;
    x.library
        .set_cached_page_count(&active, x.manifest.page_ids.len());
    Ok(())
}

fn open_notebook_from_ui(
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
    notebook_id: &str,
) -> Result<(), String> {
    crate::launch_clock::mark("notebook_open_start");
    if ctrl.store().is_bound()
        && notebook_id == x.library.active_notebook_id()
        && notebook_id == x.manifest.notebook_id
    {
        let title = x.library.title(notebook_id)?.to_string();
        ctrl.transport_mut().set_chrome_title(&title);
        x.ui.enter_notebook();
        present_ui(x, ctrl, None)
            .map_err(|e| format!("FATAL: notebook open presentation failed: {e}"))?;
        println!("notebook_opened notebook={notebook_id} manifest_write=false app_view=notebook");
        crate::launch_clock::mark("notebook_open_end");
        return Ok(());
    }
    let already_active = notebook_id == x.library.active_notebook_id();
    let commit = if already_active {
        SwitchNotebookCommit {
            from_notebook_id: notebook_id.to_string(),
            target_notebook_id: notebook_id.to_string(),
            manifest_persist_us: 0,
        }
    } else {
        match switch_notebook_durable(x.library, notebook_id, &SwitchNotebookHooks::default()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("notebook_open_failed notebook={notebook_id} detail={e}");
                x.ui.show_error("This notebook couldn't be opened.");
                return present_ui(x, ctrl, None);
            }
        }
    };
    crate::launch_clock::mark("notebook_bind_start");
    let paper = paper_background(&x.geo);
    let opened = match open_active_notebook(x.library, x.ink_request, &x.geo, &paper) {
        Ok(v) => v,
        Err(e) if already_active => {
            eprintln!("notebook_open_failed notebook={notebook_id} detail={e}");
            x.ui.show_error("This notebook couldn't be opened.");
            return present_ui(x, ctrl, None);
        }
        Err(e) => {
            return Err(format!(
                "FATAL: notebook switch committed but target load failed: {e}"
            ));
        }
    };
    let active = x.library.active_notebook_id().to_string();
    let title = x.library.title(&active)?.to_string();
    install_opened_notebook(
        &active,
        &title,
        opened.0,
        opened.1,
        ctrl,
        ActiveNotebookBindings {
            notebook_dir: x.notebook_dir,
            manifest: x.manifest,
            profile: x.notebook_profile,
            background: x.current_bg,
        },
    )?;
    x.library
        .set_cached_page_count(&active, x.manifest.page_ids.len());
    crate::launch_clock::mark("notebook_bind_end");
    x.ui.enter_notebook();
    println!(
        "notebook_switched from={} to={} manifest_persist_us={} app_view=notebook",
        commit.from_notebook_id, commit.target_notebook_id, commit.manifest_persist_us
    );
    crate::launch_clock::mark("notebook_open_end");
    Ok(())
}

fn fill_picker_thumbs(library: &LibraryRepository, picker: &mut LinkPickerState, geo: &FbGeometry) {
    use crate::link_picker::LinkPickerStage;
    let notebook_local = match &picker.stage {
        LinkPickerStage::Pages { notebook_local, .. } => notebook_local.clone(),
        LinkPickerStage::Notebooks { .. } => {
            picker.thumbs.clear();
            return;
        }
    };
    let dir = library.notebook_dir(&notebook_local);
    let ids: Vec<String> = picker.visible_page_ids().to_vec();
    picker.thumbs.clear();
    for id in ids {
        let pixels = crate::page_thumb::load_or_build(&dir, &id, geo.xres, geo.yres);
        picker.thumbs.push((
            id,
            pixels,
            crate::page_thumb::THUMB_W,
            crate::page_thumb::THUMB_H,
        ));
    }
}

fn bootstrap_identity(
    library: &LibraryRepository,
) -> Result<sync_protocol::IdentityFileV1, String> {
    let (from, plan) = crate::sync_local::plan_sync_identity(library.root())?;
    crate::sync_local::persist_planned_identity(library.root(), from.as_ref(), &plan)?;
    Ok(plan.identity)
}

fn leave_link_picker(x: &mut UiExec<'_>, ctrl: &mut KindleCtrl) -> Result<(), String> {
    x.ui.link_picker = None;
    if ctrl.store().is_bound() && !x.manifest.notebook_id.is_empty() {
        x.ui.enter_notebook();
    } else {
        let total = x.library.manifest().notebook_ids.len();
        x.ui.enter_home(active_notebook_index(x.library), total);
    }
    present_ui(x, ctrl, None)
}

fn begin_create_link(
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
    source_x: f32,
    source_y: f32,
) -> Result<(), String> {
    if !ctrl.store().is_bound() || x.manifest.notebook_id.is_empty() {
        return present_ui(x, ctrl, None);
    }
    let identity = match bootstrap_identity(x.library) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("link_identity_failed detail={e}");
            x.ui.show_error("Couldn't start a link.");
            return present_ui(x, ctrl, None);
        }
    };
    let nb_local = x.manifest.notebook_id.clone();
    let page_local = ctrl.session.page_id().to_string();
    let Some(source_notebook_id) = identity.notebook_id(&nb_local) else {
        x.ui.show_error("Couldn't start a link.");
        return present_ui(x, ctrl, None);
    };
    let Some(source_page_id) = identity.page_id(&nb_local, &page_local) else {
        x.ui.show_error("Couldn't start a link.");
        return present_ui(x, ctrl, None);
    };
    x.ui.enter_link_picker(LinkPickerState::notebooks(PendingLink {
        source_notebook_local: nb_local,
        source_page_local: page_local,
        source_notebook_id,
        source_page_id,
        source_x,
        source_y,
    }));
    println!("app_view=link_picker");
    present_ui(x, ctrl, None)
}

fn open_picker_notebook(
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
    notebook_local: &str,
) -> Result<(), String> {
    let Some(picker) = x.ui.link_picker.as_mut() else {
        return leave_link_picker(x, ctrl);
    };
    let dir = x.library.notebook_dir(notebook_local);
    match crate::library::read_notebook_manifest_read_only(&dir) {
        Ok(m) => {
            let title = x
                .library
                .title(notebook_local)
                .unwrap_or(notebook_local)
                .to_string();
            picker.show_pages(notebook_local.to_string(), title, m.page_ids);
            present_ui(x, ctrl, None)
        }
        Err(e) => {
            eprintln!("link_picker_notebook_failed detail={e}");
            x.ui.show_error("That notebook couldn't be opened.");
            present_ui(x, ctrl, None)
        }
    }
}

fn confirm_page_link(
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
    target_nb_local: &str,
    target_page_local: &str,
) -> Result<(), String> {
    let Some(pending) = x.ui.link_picker.as_ref().map(|p| p.pending.clone()) else {
        return leave_link_picker(x, ctrl);
    };
    if pending.source_notebook_local != x.manifest.notebook_id
        || pending.source_page_local != ctrl.session.page_id()
        || !x
            .library
            .manifest()
            .notebook_ids
            .iter()
            .any(|id| id == &pending.source_notebook_local)
        || !x
            .manifest
            .page_ids
            .iter()
            .any(|p| p == &pending.source_page_local)
    {
        eprintln!("link_abort reason=source_gone");
        return leave_link_picker(x, ctrl);
    }
    let identity = match bootstrap_identity(x.library) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("link_identity_failed detail={e}");
            x.ui.show_error("The link couldn't be saved.");
            return leave_link_picker(x, ctrl);
        }
    };
    let Some(target_notebook_id) = identity.notebook_id(target_nb_local) else {
        x.ui.show_error("That page is no longer available.");
        return present_ui(x, ctrl, None);
    };
    let Some(target_page_id) = identity.page_id(target_nb_local, target_page_local) else {
        x.ui.show_error("That page is no longer available.");
        return present_ui(x, ctrl, None);
    };
    let link = sync_protocol::SyncPageLink {
        link_id: sync_protocol::LinkId::from_uuid(uuid::Uuid::now_v7())
            .map_err(|e| e.to_string())?,
        source_notebook_id: pending.source_notebook_id,
        source_page_id: pending.source_page_id,
        source_x: pending.source_x,
        source_y: pending.source_y,
        target_notebook_id,
        target_page_id,
    };
    if let Err(e) = link.validate() {
        eprintln!("link_invalid detail={e}");
        x.ui.show_error("That page can't be linked.");
        return present_ui(x, ctrl, None);
    }
    let dir = page_dir(x.notebook_dir, &pending.source_page_local);
    let source_nb = pending.source_notebook_local.clone();
    let source_pg = pending.source_page_local.clone();
    let target_nb = target_nb_local.to_string();
    let target_pg = target_page_local.to_string();
    let saved = {
        let library = &*x.library;
        let author = |sync: Option<&crate::sync_outbound::OutboundSync>| -> Result<
            Option<Vec<sync_protocol::SyncPageLink>>,
            String,
        > {
            crate::sync_outbound::author_add_page_link(
                sync,
                &source_nb,
                &source_pg,
                &dir,
                &link,
                || crate::link_picker::selected_target_is_live(library, &target_nb, &target_pg),
            )
        };
        if let Some(result) = crate::sync_outbound::with_global(|s| author(Some(s))) {
            result?
        } else {
            author(None)?
        }
    };
    let Some(links) = saved else {
        x.ui.show_error("That page is no longer available.");
        return present_ui(x, ctrl, None);
    };
    ctrl.replace_links(links.clone());
    ctrl.transport_mut().install_page_links(links);
    x.ui.link_picker = None;
    x.ui.enter_notebook();
    println!(
        "page_link_added link={} from={}/{} to={}/{}",
        link.link_id,
        pending.source_notebook_local,
        pending.source_page_local,
        target_nb_local,
        target_page_local
    );
    present_ui(x, ctrl, None)
}

fn open_link_popup(
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
    link_id: sync_protocol::LinkId,
) -> Result<(), String> {
    let Some(link) = ctrl.links().iter().find(|l| l.link_id == link_id).cloned() else {
        return present_ui(x, ctrl, None);
    };
    let identity = crate::sync_local::read_identity(x.library.root())?;
    let live = identity.as_ref().and_then(|id| {
        crate::link_picker::resolve_live_target(
            x.library,
            id,
            link.target_notebook_id,
            link.target_page_id,
        )
    });
    let (label, target_live) = crate::link_picker::link_jump_presentation(live.as_ref());
    let (notebook_local, page_local) = match &live {
        Some(t) => (Some(t.notebook_local.clone()), Some(t.page_local.clone())),
        None => (None, None),
    };
    x.ui.modal = Some(Modal::LinkJump {
        link_id,
        label,
        target_live,
        notebook_local,
        page_local,
    });
    present_ui(x, ctrl, None)
}

fn jump_to_page(
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
    notebook_local: &str,
    page_local: &str,
) -> Result<(), String> {
    x.ui.modal = None;
    if notebook_local != x.library.active_notebook_id() {
        open_notebook_from_ui(x, ctrl, notebook_local)?;
    } else if x.ui.view != AppView::Notebook {
        x.ui.enter_notebook();
    }
    if ctrl.session.page_id() == page_local {
        return present_ui(x, ctrl, None);
    }
    bind_existing_page(
        x.notebook_dir,
        x.manifest,
        ctrl,
        x.notebook_profile,
        x.current_bg,
        page_local,
    )?;
    present_ui(x, ctrl, None)
}

fn bind_existing_page(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    ctrl: &mut KindleCtrl,
    notebook_profile: &RenderProfile,
    current_bg: &mut Vec<u8>,
    target_id: &str,
) -> Result<(), String> {
    if !manifest.page_ids.iter().any(|p| p == target_id) {
        return Err(format!("jump target {target_id} not in notebook"));
    }
    let geo = ctrl.transport().geometry();
    let (new_store, loaded) = load_page_for_switch(data_dir, target_id, notebook_profile, &geo)?;
    let chrome = chrome_for_page(manifest, target_id)?;
    let strokes = loaded.strokes.clone();
    let roots = loaded.root_of.clone();
    let bg = loaded.background.clone();
    let (texts, links) = load_and_paint_page_content(ctrl.transport_mut(), data_dir, target_id)?;
    ctrl.transport_mut()
        .reconstruct_page(&bg, &strokes, Some(chrome))?;
    ctrl.bind_page_with_roots(
        BoundPageKey::new(&manifest.notebook_id, target_id),
        strokes,
        roots,
        RuntimeStore::Bound(new_store),
        texts,
        links,
    )
    .map_err(|e| format!("FATAL: jump bind failed: {e}"))?;
    persist_active_page(data_dir, manifest, target_id)?;
    *current_bg = bg;
    ctrl.clear_pending_clean();
    Ok(())
}

fn remove_page_link(
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
    link_id: sync_protocol::LinkId,
) -> Result<(), String> {
    x.ui.modal = None;
    let page_local = ctrl.session.page_id().to_string();
    let notebook_local = x.manifest.notebook_id.clone();
    let dir = page_dir(x.notebook_dir, &page_local);
    let saved = {
        let author = |sync: Option<&crate::sync_outbound::OutboundSync>| -> Result<
            Option<Vec<sync_protocol::SyncPageLink>>,
            String,
        > {
            crate::sync_outbound::author_delete_page_link(
                sync,
                &notebook_local,
                &page_local,
                &dir,
                link_id,
            )
        };
        if let Some(result) = crate::sync_outbound::with_global(|s| author(Some(s))) {
            result?
        } else {
            author(None)?
        }
    };
    let Some(links) = saved else {
        return present_ui(x, ctrl, None);
    };
    ctrl.replace_links(links.clone());
    ctrl.transport_mut().install_page_links(links);
    println!("page_link_removed link={link_id} page={page_local}");
    present_ui(x, ctrl, None)
}

/// Execute one UI command. `Err` is fatal (fail closed); recoverable failures become an
/// on-screen error dialog with details only in the log.
fn execute_ui_command(
    cmd: UiCommand,
    x: &mut UiExec<'_>,
    ctrl: &mut KindleCtrl,
) -> Result<(), String> {
    match cmd {
        UiCommand::None => Ok(()),
        UiCommand::Redraw(Redraw::Full) => present_ui(x, ctrl, None),
        UiCommand::Redraw(Redraw::Region(r)) => present_ui(x, ctrl, Some(r)),
        UiCommand::OpenTools => {
            x.ui.enter_tools();
            println!("app_view=tools");
            present_ui(x, ctrl, None)
        }
        UiCommand::ShowToolsTab => {
            x.ui.show_tools_tab();
            println!("app_view=tools tab=tools");
            present_ui(x, ctrl, None)
        }
        UiCommand::OpenPreferences => {
            x.ui.enter_preferences();
            println!("app_view=preferences");
            present_ui(x, ctrl, None)
        }
        UiCommand::SetSideButton(action) => {
            let update = prefs::apply_side_button(x.ui.side_button, x.prefs_path, action);
            x.ui.side_button = update.action;
            if let Some(e) = update.error {
                eprintln!("preferences_save_failed detail={e}");
                x.ui.show_error("The side button setting couldn't be saved.");
            } else {
                println!("side_button={:?}", update.action);
            }
            present_ui(x, ctrl, None)
        }
        UiCommand::BeginCreateLink { x: px, y: py } => begin_create_link(x, ctrl, px, py),
        UiCommand::LinkPickerCancel => leave_link_picker(x, ctrl),
        UiCommand::LinkPickerOpenNotebook(id) => open_picker_notebook(x, ctrl, &id),
        UiCommand::LinkPickerSelectPage {
            notebook_local,
            page_local,
        } => confirm_page_link(x, ctrl, &notebook_local, &page_local),
        UiCommand::OpenLinkPopup { link_id } => open_link_popup(x, ctrl, link_id),
        UiCommand::JumpLink {
            notebook_local,
            page_local,
        } => jump_to_page(x, ctrl, &notebook_local, &page_local),
        UiCommand::RemoveLink { link_id } => remove_page_link(x, ctrl, link_id),
        UiCommand::ToolsScan => {
            x.ui.tools.phase = tools_scan_phase(x.library).0;
            present_ui(x, ctrl, None)
        }
        UiCommand::ToolsImportAll => {
            // Re-scan at the moment of import: never act on a stale list.
            let (phase, report) = tools_scan_phase(x.library);
            x.ui.tools.phase = phase;
            if let Some(r) = report {
                let queue: std::collections::VecDeque<_> =
                    r.eligible().into_iter().cloned().collect();
                if let Some(first) = queue.front() {
                    x.ui.tools.phase = crate::tools_ui::ToolsPhase::Importing {
                        done: 0,
                        total: queue.len(),
                        current: first.title.clone(),
                    };
                    println!(
                        "nbk_import_start sources={} already_imported={}",
                        queue.len(),
                        r.already_imported()
                    );
                    *x.import_run = Some(ImportRun {
                        total: queue.len(),
                        queue,
                        imported: 0,
                        skipped: r.already_imported(),
                        failures: Vec::new(),
                    });
                }
            }
            present_ui(x, ctrl, None)
        }
        UiCommand::OpenSync => {
            x.ui.enter_sync();
            crate::server_link::with_global(|l| {
                l.send(crate::server_link::LinkCmd::EnterSyncScreen)
            });
            println!("app_view=sync");
            present_ui(x, ctrl, None)
        }
        UiCommand::SyncPair(id) => {
            crate::server_link::with_global(|l| l.send(crate::server_link::LinkCmd::Pair(id)));
            println!("sync_pair_requested server_id={id}");
            Ok(())
        }
        UiCommand::SyncCancelPairing => {
            crate::server_link::with_global(|l| l.send(crate::server_link::LinkCmd::CancelPairing));
            Ok(())
        }
        UiCommand::SyncManualUrl(url) => {
            crate::server_link::with_global(|l| {
                l.send(crate::server_link::LinkCmd::ManualUrl(url))
            });
            present_ui(x, ctrl, None)
        }
        UiCommand::SyncRestoreBegin => {
            // 1H-D2: the server link runs the whole flow; this installation is what it replaces.
            // 1H-D3: only in builds that offer it (the button is not drawn otherwise).
            if let Some(sync_dir) = x.sync_dir.filter(|_| crate::sync_ui::RESTORE_FROM_SERVER) {
                let ctx = crate::server_link::RestoreContext {
                    library_root: x.library.root().to_path_buf(),
                    sync_dir: sync_dir.to_path_buf(),
                    device: crate::sync_outbound::DeviceGeometry {
                        width: x.geo.xres,
                        height: x.geo.yres,
                        line_length: x.geo.line_length,
                    },
                };
                crate::server_link::with_global(|l| {
                    l.send(crate::server_link::LinkCmd::BeginRestore(ctx))
                });
                println!("sync_restore=begin");
            }
            Ok(())
        }
        cmd @ (UiCommand::SyncRestoreServer(_)
        | UiCommand::SyncRestoreLibrary(_)
        | UiCommand::SyncRestoreConfirm
        | UiCommand::SyncRestoreCancel
        | UiCommand::SyncRestoreDismiss) => {
            if let Some(link_cmd) = cmd.restore_link_cmd() {
                println!("sync_restore_step={link_cmd:?}");
                crate::server_link::with_global(|l| l.send(link_cmd));
            }
            Ok(())
        }
        UiCommand::GoHome => {
            if x.ui.view == AppView::Sync {
                crate::server_link::with_global(|l| {
                    l.send(crate::server_link::LinkCmd::LeaveSyncScreen)
                });
            }
            ctrl.clear_pending_clean();
            let total = x.library.manifest().notebook_ids.len();
            x.ui.enter_home(active_notebook_index(x.library), total);
            present_ui(x, ctrl, None)?;
            println!(
                "app_view=home selected_notebook={} active_notebook={}",
                x.library.manifest().notebook_ids[x.ui.home.selected_index],
                x.library.active_notebook_id()
            );
            Ok(())
        }
        UiCommand::ExitApp => request_app_exit(x, ctrl),
        UiCommand::OpenNotebook(id) => open_notebook_from_ui(x, ctrl, &id),
        UiCommand::CreateFolder(title) => folder_mutation(
            x,
            ctrl,
            "created",
            "The folder couldn't be created.",
            |f, live| f.create(&title, live).map(FolderCapture::Created),
        ),
        UiCommand::RenameFolder { folder_id, title } => folder_mutation(
            x,
            ctrl,
            "renamed",
            "The folder couldn't be renamed.",
            |f, live| {
                f.rename(&folder_id, &title, live)?;
                Ok(FolderCapture::Renamed(folder_id.clone()))
            },
        ),
        UiCommand::DeleteFolder(folder_id) => {
            if x.ui.home_folder.as_deref() == Some(folder_id.as_str()) {
                x.ui.home_folder = None;
            }
            folder_mutation(
                x,
                ctrl,
                "deleted",
                "The folder couldn't be deleted.",
                |f, live| {
                    // Resolve the global identity while the folder still exists.
                    let gid = crate::sync_outbound::with_global(|s| {
                        f.sync_id(&folder_id, s.library_id())
                    })
                    .flatten();
                    f.delete(&folder_id, live)?;
                    Ok(match gid {
                        Some(g) => FolderCapture::Deleted(g),
                        None => FolderCapture::Nothing,
                    })
                },
            )
        }
        UiCommand::SetNotebookFolder {
            notebook_id,
            folder,
        } => folder_mutation(
            x,
            ctrl,
            "membership_changed",
            "The notebook couldn't be moved.",
            |f, live| {
                f.set_folder(&notebook_id, folder.as_deref(), live)?;
                Ok(FolderCapture::Filed {
                    notebook: notebook_id.clone(),
                    folder: folder.clone(),
                })
            },
        ),
        UiCommand::CreateNotebookInFolder { title, folder_id } => {
            let paper = paper_background(&x.geo);
            match create_notebook_durable(
                x.library,
                x.notebook_profile,
                &x.geo,
                &paper,
                Some(&title),
                &CreateNotebookHooks::default(),
            ) {
                Ok(commit) => {
                    sync_notebook_created(x.library, &commit.notebook_id, &commit.title);
                    println!(
                        "notebook_created notebook={} title={:?} manifest_persist_us={} active_notebook={} source=ui folder={folder_id}",
                        commit.notebook_id,
                        commit.title,
                        commit.manifest_persist_us,
                        x.library.active_notebook_id()
                    );
                    // The notebook exists either way; a filing failure only leaves it on the
                    // main list.
                    let live = x.library.manifest().notebook_ids.clone();
                    match x
                        .folders
                        .set_folder(&commit.notebook_id, Some(&folder_id), &live)
                    {
                        Ok(()) => capture_folder_change(
                            x.folders,
                            FolderCapture::Filed {
                                notebook: commit.notebook_id.clone(),
                                folder: Some(folder_id.clone()),
                            },
                        ),
                        Err(e) => eprintln!(
                            "folder_membership_changed_failed notebook={} detail={e}",
                            commit.notebook_id
                        ),
                    }
                    x.ui.enter_notebook();
                    install_active_notebook(x, ctrl).map_err(|e| {
                        format!("FATAL: notebook create committed but open failed: {e}")
                    })
                }
                Err(e) => {
                    eprintln!("notebook_create_failed title={title:?} detail={e}");
                    x.ui.show_error("The notebook couldn't be created.");
                    present_ui(x, ctrl, None)
                }
            }
        }
        UiCommand::CreateNotebook(title) => {
            let paper = paper_background(&x.geo);
            match create_notebook_durable(
                x.library,
                x.notebook_profile,
                &x.geo,
                &paper,
                Some(&title),
                &CreateNotebookHooks::default(),
            ) {
                Ok(commit) => {
                    sync_notebook_created(x.library, &commit.notebook_id, &commit.title);
                    println!(
                        "notebook_created notebook={} title={:?} manifest_persist_us={} active_notebook={} source=ui",
                        commit.notebook_id,
                        commit.title,
                        commit.manifest_persist_us,
                        x.library.active_notebook_id()
                    );
                    x.ui.enter_notebook();
                    install_active_notebook(x, ctrl).map_err(|e| {
                        format!("FATAL: notebook create committed but open failed: {e}")
                    })
                }
                Err(e) => {
                    eprintln!("notebook_create_failed title={title:?} detail={e}");
                    x.ui.show_error("The notebook couldn't be created.");
                    present_ui(x, ctrl, None)
                }
            }
        }
        UiCommand::RenameNotebook { notebook_id, title } => {
            let ticket = if x.library.title(&notebook_id).ok() != Some(title.as_str()) {
                match sync_prepare(|s| s.prepare_rename_notebook(&notebook_id, &title)) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("notebook_rename_failed notebook={notebook_id} detail={e}");
                        x.ui.show_error("The notebook couldn't be renamed.");
                        return present_ui(x, ctrl, None);
                    }
                }
            } else {
                None
            };
            let result = rename_notebook_durable(
                x.library,
                &notebook_id,
                &title,
                &RenameNotebookHooks::default(),
            );
            sync_resolve(ticket, result.as_ref().is_ok_and(|c| c.changed));
            match result {
                Ok(commit) => println!(
                    "notebook_renamed notebook={} old_title={:?} new_title={:?} changed={} info_persist_us={}",
                    commit.notebook_id,
                    commit.old_title,
                    commit.new_title,
                    commit.changed,
                    commit.info_persist_us
                ),
                Err(e) => {
                    eprintln!("notebook_rename_failed notebook={notebook_id} detail={e}");
                    x.ui.show_error("The notebook couldn't be renamed.");
                }
            }
            present_ui(x, ctrl, None)
        }
        UiCommand::DeleteNotebook(notebook_id) => {
            let ticket = if x.library.manifest().notebook_ids.len() >= 2 {
                match sync_prepare(|s| s.prepare_delete_notebook(&notebook_id)) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("notebook_delete_failed notebook={notebook_id} detail={e}");
                        x.ui.show_error("The notebook couldn't be deleted.");
                        return present_ui(x, ctrl, None);
                    }
                }
            } else {
                None
            };
            let result =
                delete_notebook_durable(x.library, &notebook_id, &DeleteNotebookHooks::default());
            sync_resolve(ticket, result.is_ok());
            match result {
                Ok(commit) => {
                    let quarantine = match commit.quarantine {
                        NotebookQuarantineOutcome::Moved { trash_entry_name } => trash_entry_name,
                        NotebookQuarantineOutcome::LeftAsOrphan { reason } => {
                            format!("orphan({reason})")
                        }
                    };
                    println!(
                        "notebook_deleted notebook={} active_notebook={} quarantine={} manifest_persist_us={} app_view=home source=ui",
                        commit.deleted_notebook_id,
                        commit.active_notebook_id,
                        quarantine,
                        commit.manifest_persist_us
                    );
                    let total = x.library.manifest().notebook_ids.len();
                    x.ui.enter_home(active_notebook_index(x.library), total);
                    present_ui(x, ctrl, None).map_err(|e| {
                        format!("FATAL: notebook delete committed but Home redraw failed: {e}")
                    })
                }
                Err(e) => {
                    eprintln!("notebook_delete_failed notebook={notebook_id} detail={e}");
                    if x.library.manifest().notebook_ids.len() < 2 {
                        x.ui.show_modal(Modal::sole_notebook_notice());
                    } else {
                        x.ui.show_error("The notebook couldn't be deleted.");
                    }
                    present_ui(x, ctrl, None)
                }
            }
        }
        UiCommand::Undo => {
            ctrl.totals.undo_gestures += 1;
            undo_and_present(
                ctrl,
                x.manifest,
                x.current_bg,
                x.lasso,
                x.overlay,
                x.last_move_undo,
                x.cached_move_ids,
            )
        }
        UiCommand::PreviousPage | UiCommand::NextPage => {
            let action = if cmd == UiCommand::PreviousPage {
                NavDownAction::Prev
            } else {
                NavDownAction::NextOrCreate
            };
            navigate(
                x.notebook_dir,
                x.manifest,
                ctrl,
                x.notebook_profile,
                x.current_bg,
                action,
            )
            .map(|_| ())
            .map_err(|e| format!("error: page navigation: {e}"))
        }
        UiCommand::InsertPageAfter => {
            if !insert_page_after_current(
                x.notebook_dir,
                x.manifest,
                ctrl,
                x.notebook_profile,
                x.current_bg,
            )? {
                x.ui.show_error("A page couldn't be inserted.");
            }
            // Success already reconstructed; a refusal must repaint the (closed) menu away.
            if x.ui.modal_open() {
                present_ui(x, ctrl, None)?;
            }
            Ok(())
        }
        UiCommand::MovePageLeft | UiCommand::MovePageRight => {
            let direction = if cmd == UiCommand::MovePageLeft {
                MovePageDirection::Left
            } else {
                MovePageDirection::Right
            };
            if !move_current_page(x.notebook_dir, x.manifest, ctrl, x.current_bg, direction)? {
                x.ui.show_error("The page couldn't be moved.");
                present_ui(x, ctrl, None)?;
            }
            Ok(())
        }
        UiCommand::DeletePage => {
            if !delete_current_page(
                x.notebook_dir,
                x.manifest,
                ctrl,
                x.notebook_profile,
                x.current_bg,
            )? {
                if x.manifest.page_ids.len() < 2 {
                    x.ui.show_modal(Modal::sole_page_notice());
                } else {
                    x.ui.show_error("The page couldn't be deleted.");
                }
                present_ui(x, ctrl, None)?;
            }
            Ok(())
        }
    }
}

/// Live framebuffer session. The process singleton must already be held; this
/// function mmaps `/dev/fb0` and opens input.
pub fn run_page_session(cfg: RuntimeConfig, stop: Arc<AtomicBool>) -> i32 {
    crate::launch_clock::mark("runtime_entry");
    println!(
        "# SCRIBE APP GATE F11: V1 usable notebook UX (Home, modals, keyboard, chrome) + F0–F10"
    );
    println!("hardening=F0.1 fail-closed (mandatory EVIOCGRAB; disarm failure is fatal)");
    println!(
        "du_interval_us={} du_override={} target_hz≈{:.1}",
        cfg.du_interval_us,
        cfg.du_override,
        du_target_hz(cfg.du_interval_us)
    );
    println!("ink_request={:?}", cfg.ink_request);
    println!("data_dir={}", cfg.data_dir.display());
    println!(
        "nav=side+tip: outer 15% = page nav; center 50% = undo; intervening guard bands = no-op"
    );
    println!(
        "eraser=BTN_TOOL_RUBBER partial-stroke capsule sweep radius_canonical={ERASER_HIT_RADIUS_CANONICAL} (commit-on-Up)"
    );
    println!("undo=writing tip + side + center → newest visible stroke (not action history)");
    println!(
        "page_order=dev triggers `{DEV_MOVE_PAGE_LEFT_TRIGGER}` / `{DEV_MOVE_PAGE_RIGHT_TRIGGER}` / `{DEV_INSERT_PAGE_AFTER_TRIGGER}` (idle only; priority L>R>insert>delete)"
    );
    println!(
        "page_delete=dev trigger file `{DEV_DELETE_CURRENT_PAGE_TRIGGER}` (idle only; refuses sole page)"
    );
    println!(
        "library=dev triggers `{DEV_HOME_TRIGGER}` / `{DEV_LIBRARY_NEXT_TRIGGER}` / `{DEV_LIBRARY_PREV_TRIGGER}` / `{DEV_LIBRARY_OPEN_TRIGGER}` / `{DEV_CREATE_NOTEBOOK_TRIGGER}` / `{DEV_DELETE_NOTEBOOK_TRIGGER}` (idle only; priority home>next>prev>open>create>delete)"
    );
    println!("dev_triggers=developer-only (diagnostics); product UI is pen-operated Home + chrome");
    println!("ui=launch Home; pen taps: modal > notebook chrome > Home > page ink");
    println!(
        "notebook=fullscreen (F11.3): finger tap reveals chrome; auto-hide {} ms; content tap / pen Down / swipe hides; stylus proximity suppresses touch",
        crate::fullscreen::CHROME_AUTO_HIDE_MS
    );
    println!("exit=Ctrl+C (SIGKILL/power-loss bypass userspace cleanup)");

    if !cfg.confirm_write {
        println!("confirm_write=missing");
        println!("REFUSE PHYSICAL WRITE");
        return 1;
    }

    let info = match LinuxFramebuffer::open_info_only_path(&cfg.fb_path) {
        Ok(fb) => fb,
        Err(e) => {
            eprintln!("error: read-only fb probe failed: {e}");
            return 1;
        }
    };
    if let Err(mismatches) = gate_b::check_preconditions(&info.fix, &info.var) {
        eprintln!("REFUSE PHYSICAL WRITE: Gate B framebuffer preconditions failed:");
        for m in mismatches {
            eprintln!("  mismatch: {m}");
        }
        return 1;
    }
    println!("preconditions=ok (hwtcon_v2 Gate B snapshot)");

    let mut fb = match MappedFramebuffer::open_path(&cfg.fb_path) {
        Ok(fb) => fb,
        Err(e) => {
            eprintln!("error: open/mmap: {e}");
            return 1;
        }
    };
    let g = fb.geometry;
    crate::launch_clock::mark("framebuffer_ready");

    crate::launch_clock::mark("loading_paint_submit");
    match present_loading_splash(&mut fb, &g) {
        Ok((marker, collision)) => {
            crate::launch_clock::mark("loading_visible");
            println!("loading_startup_gc16_full marker={marker} collision={collision}");
        }
        Err(e) => {
            eprintln!("loading_startup_failed detail={e}");
        }
    }

    let mut raw_src = match LinuxEvdevStylus::open_nonblocking(&cfg.stylus_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: open stylus {}: {e}", cfg.stylus_path);
            return abort_after_loading(e);
        }
    };
    let grab_ok = match raw_src.grab_exclusive() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("EVIOCGRAB failed: {e}");
            false
        }
    };
    if let Err(e) = require_eviocgrab(grab_ok) {
        eprintln!("{e}");
        drop(raw_src);
        drop(fb);
        println!("eviocgrab=not_held");
        return abort_after_loading(e);
    }
    println!(
        "grab=EVIOCGRAB once at start (held for session; path={})",
        cfg.stylus_path
    );
    println!("input_pump=dedicated_thread (drains while main does WAIT/fsync/idle-GC16)");
    // F11.2: finger UI/page swipes. Observer only: plain read, NEVER EVIOCGRAB, so Kindle
    // system gestures keep the same stream. Any failure leaves Scribe pen-only.
    let mut touch: Option<(TouchPump, TouchMapping)> = None;
    if let Some(path) = cfg.touch_path.as_deref() {
        match LinuxEvdevTouch::open_observer(path) {
            Ok(t) => {
                let axes = t.axes();
                match TouchMapping::new((axes.x.min, axes.x.max), (axes.y.min, axes.y.max), &g) {
                    Ok(mapping) => {
                        println!(
                            "touch=observer path={path} grab=never x={}..{} y={}..{} slot_max={}",
                            axes.x.min, axes.x.max, axes.y.min, axes.y.max, axes.slot_max
                        );
                        touch = Some((TouchPump::start(t), mapping));
                    }
                    Err(e) => println!("touch=disabled reason={e}"),
                }
            }
            Err(e) => println!("touch=unavailable path={path} err={e}"),
        }
    } else {
        println!("touch=disabled reason=--no-touch");
    }

    let input_clock = raw_src.kernel_clock();
    println!("input_kernel_clock={input_clock:?}");
    match raw_src.axes().pressure {
        Some(pr) => println!(
            "pressure_axis=ABS_PRESSURE raw_min={} raw_max={} semantic=normalized_0_1 \
             via AxisRange::normalize (clamp)",
            pr.min, pr.max
        ),
        None => println!("pressure_axis=absent (StylusEvent.pressure will be None)"),
    }
    if cfg.trace_interstroke {
        println!("trace_interstroke=true");
    }
    if cfg.trace_pressure {
        println!("trace_pressure=true");
    }
    let display_counters = Arc::new(DisplayEventCounters::default());
    if cfg.display_events_stdin {
        spawn_stdin_reader(Arc::clone(&display_counters), Arc::clone(&stop));
        println!("display_events=stdin relay (goingToScreenSaver→invalidate, exitingScreenSaver→restore)");
    } else if crate::display_events::spawn_powerd_watcher(Arc::clone(&display_counters)) {
        // Not hosted (e.g. an SSH-launched session): watch the same two powerd events directly.
        println!("display_events=powerd watcher (lipc-wait-event; goingToScreenSaver→invalidate, exitingScreenSaver→restore)");
    } else {
        println!("display_events=none (first-contact fail-safe only)");
    }
    let mut display_rx = DisplayEventReceiver::new(Arc::clone(&display_counters));

    // Move exclusively-owned grabbed source into the input pump thread.
    let mut src = InputPump::start(raw_src);
    crate::launch_clock::mark("device_init_done");

    // F11: a brand-new library/notebook gets plain paper. The live framebuffer at launch shows
    // Amazon UI (and later Home/modals/chrome), so it is never a durable background source.
    let bg_capture = paper_background(&g);

    // 1H-D1: finish (or discard) an interrupted library adoption before the library is opened,
    // so neither an empty new library nor sync is ever started over a half-published one.
    if let Some(sync_dir) = &cfg.sync_dir {
        match crate::sync_adopt::recover_interrupted_adoption(
            &cfg.data_dir,
            sync_dir,
            cfg.device_dir.as_deref(),
        ) {
            Ok(crate::sync_adopt::AdoptionRecovery::None) => {}
            Ok(r) => println!("adoption_recovery={r:?}"),
            Err(e) => {
                let e = format!("REFUSE LIBRARY OPEN: interrupted library adoption: {e}");
                eprintln!("{e}");
                drop(src);
                return abort_after_loading(e);
            }
        }
    }

    crate::launch_clock::mark("library_open_start");
    let library_open =
        match open_or_create_library_shell(&cfg.data_dir, cfg.ink_request, &g, &bg_capture) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("{e}");
                drop(src);
                return abort_after_loading(e);
            }
        };
    crate::launch_clock::mark("library_open_end");

    let mut library = library_open.repository;
    let mut notebook_dir = PathBuf::new();
    let mut manifest = PersistedNotebookManifestV1 {
        v: NOTEBOOK_MANIFEST_VERSION,
        notebook_id: String::new(),
        page_ids: Vec::new(),
        active_page_id: String::new(),
        next_page_ordinal: 1,
    };
    let mut notebook_profile =
        match RenderProfile::sealed_v2_pressure(crate::ink::ink_for_new_page(cfg.ink_request)) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                drop(src);
                return abort_after_loading(e);
            }
        };
    let initial_active_page = String::new();
    let mut current_bg: Vec<u8> = Vec::new();

    // S2B: outbound replication only with an explicit `--sync-dir` (off by default). Sync
    // problems never block the notebook: a refusal just leaves sync off.
    crate::launch_clock::mark("sync_reconcile_begin");
    if let Some(sync_dir) = &cfg.sync_dir {
        // The framebuffer geometry is known here: supplying it skips the first-page probe and is
        // also what startup recovery of an interrupted inbound apply materializes with.
        let device = crate::sync_outbound::DeviceGeometry {
            width: g.xres,
            height: g.yres,
            line_length: g.line_length,
        };
        // Slice 19B.2B: on a proven verified-fast-path start the confirmed base loads in the
        // background; `open_with` still returns only once local capture is safe, and the
        // global is installed before Home becomes interactive either way.
        let opts = crate::sync_outbound::SyncOpenOptions {
            device: Some(device),
            defer_confirmed_base: crate::sync_outbound::DEFER_CONFIRMED_BASE,
            ..Default::default()
        };
        match crate::sync_outbound::OutboundSync::open_with(&cfg.data_dir, sync_dir, opts) {
            Ok(s) => {
                println!("sync_enabled=true sync_dir={}", sync_dir.display());
                crate::sync_outbound::install_global(s);
                crate::launch_clock::mark("sync_capture_ready");
            }
            Err(e) => println!("sync_enabled=false reason={e:?}"),
        }
    }
    crate::launch_clock::mark("sync_reconcile_end");
    // D1 product mode: the paired-server link (discovery, pairing, per-device credential).
    // Developer mode (`<sync-dir>/config.json`) is unchanged and takes precedence.
    let mut product_sync = false;
    if let (Some(sync_dir), Some(device_dir)) = (&cfg.sync_dir, &cfg.device_dir) {
        let dev_config = sync_dir.join(crate::sync_outbound::CONFIG_FILE).exists();
        let sync_on = crate::sync_outbound::with_global(|_| ()).is_some();
        if sync_on && !dev_config {
            // Remove any stale discovery aperture left by a crashed previous run.
            crate::discovery::Aperture::for_this_device().close();
            let aperture = std::sync::Arc::new(crate::discovery::Aperture::for_this_device());
            match crate::server_link::ServerLink::start(
                device_dir,
                crate::server_link::GlobalSync,
                crate::server_link::MdnsSource::new(aperture),
                "Ignite",
            ) {
                Ok(link) => {
                    crate::server_link::install_global(link);
                    product_sync = true;
                    println!("product_sync=true device_dir={}", device_dir.display());
                }
                Err(e) => println!("product_sync=false reason={e:?}"),
            }
        } else {
            println!(
                "product_sync=false reason={}",
                if dev_config {
                    "developer config.json present"
                } else {
                    "sync off"
                }
            );
        }
    }

    // NBK import: a crash mid-import leaves a `pending` receipt. Remove its partial copy before
    // any UI can open it (never two copies of one Amazon notebook); the source stays retryable.
    if crate::nbk_import::Ledger::path_for(library.root()).exists() {
        match crate::nbk_import::recover_interrupted(&mut library, &crate::nbk_import::GlobalSync) {
            Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
            Err(e) => eprintln!("nbk_import_recovery_failed detail={e}"),
        }
    }

    if library_open.migrated_legacy {
        println!("legacy_library_migration=true");
    }
    if !library.orphan_dirs().is_empty() {
        println!("orphan_notebook_dirs={:?}", library.orphan_dirs());
    }
    println!(
        "library_notebooks={} active_notebook_id={} next_notebook_ordinal={}",
        library.manifest().notebook_ids.len(),
        library.active_notebook_id(),
        library.manifest().next_notebook_ordinal
    );
    println!("notebook_bind=deferred_until_open");
    println!(
        "render_profile version={} ink={:02x} brush_radius={} transform={} rasterizer={}",
        notebook_profile.profile_version,
        notebook_profile.ink_byte,
        notebook_profile.brush_radius,
        notebook_profile.transform_id,
        notebook_profile.rasterizer_id
    );

    let mut transport = KindleTransport::new(fb, notebook_profile.clone());
    match library.title(library.active_notebook_id()) {
        Ok(t) => transport.set_chrome_title(t),
        Err(e) => {
            eprintln!("error: {e}");
            drop(src);
            return abort_after_loading(e);
        }
    }
    // F11: launch into Home; bind the notebook only when the user opens one.
    crate::launch_clock::mark("home_cards_start");
    let launch_cards = home_cards(&library);
    crate::launch_clock::mark("home_cards_end");
    let mut ui = UiState::launch_home(active_notebook_index(&library), launch_cards.len());
    let prefs_path = prefs::preferences_path(cfg.sync_dir.as_deref(), cfg.device_dir.as_deref());
    ui.side_button = prefs::load_side_button(&prefs_path);
    if product_sync {
        ui.enable_sync();
    }
    let mut folders = FolderStore::open(library.root());
    if let Some(e) = folders.unreadable() {
        eprintln!("folders_unreadable detail={e} (folders are read-only this session)");
    }
    refresh_folder_view(&mut ui, &folders, &library);
    ui.settle_home(&launch_cards);
    crate::launch_clock::mark("home_paint_submit");
    if let Err(e) = transport.render_home(&launch_cards, &ui.folders, None, &ui.home, None) {
        eprintln!("error: render Home at launch: {e}");
        drop(src);
        return abort_after_loading(e);
    }
    crate::launch_clock::mark("home_gc16_done");
    println!(
        "app_view=home launch=true selected_notebook={} active_notebook={}",
        library.manifest().notebook_ids[ui.home.selected_index],
        library.active_notebook_id()
    );
    println!("bound_strokes=0");

    let mut ctrl = StrokeController::with_store(
        "unbound",
        transport,
        cfg.du_interval_us,
        input_clock,
        RuntimeStore::Unbound,
    );

    let cal = portrait_wacom_calibration();
    let mut nav = NavGestureState::Idle;
    let mut eraser = EraserGestureState::Idle;
    let mut router = PenRouter::default();
    let mut pending_ui: Option<UiCommand> = None;
    let session_start = Instant::now();
    // F11.2/F11.3 finger policy + palm arbitration (stylus proximity poisons contacts).
    let mut touch_arb = TouchArbiter::default();
    let mut lasso = LassoState::default();
    let mut overlay = TouchOverlayDirty::default();
    let mut last_move_undo: Option<MoveCommit> = None;
    let mut cached_move_ids: Option<HashSet<String>> = None;
    let mut move_coalesce = MovePreviewCoalesce::default();
    let mut logged_proximity: Option<bool> = None;
    let ms_since_start =
        |at: Instant| at.saturating_duration_since(session_start).as_millis() as u64;
    println!("ready: waiting for Pen Down / Eraser Down (Ctrl+C to exit) ...");
    crate::launch_clock::mark("ui_ready");
    crate::launch_clock::mem_sample("ui_ready");
    if let Some((pump, _)) = touch.as_ref() {
        crate::launch_diag::live().emit_ui_ready_fifo(
            pump.backlog(),
            pump.produced_seq(),
            pump.consumed_seq(),
        );
    }

    #[allow(unused_assignments)]
    let mut shutdown_safe = true;
    #[allow(unused_assignments)]
    let mut stylus_mode_disarm_failed = false;
    let mut sleep_guard: Option<crate::sleep_ui::SleepGuard> = None;
    let mut import_run: Option<ImportRun> = None;
    let mut input_lag = crate::input_lag::InputLag::default();
    let mut last_stylus_us = 0u64;
    let mut sync_capture_max_us = 0u64;
    let mut inbound_apply_count = 0u64;
    let mut inbound_apply_max_us = 0u64;
    let code = loop {
        crate::launch_diag::live().maybe_emit_stats();
        if stop.load(Ordering::SeqCst) {
            println!("shutdown=signal");
            let (c, safe, disarm_fail) = finish_shutdown(&mut ctrl, true);
            shutdown_safe = safe;
            stylus_mode_disarm_failed = disarm_fail;
            break c;
        }

        // F11.1: Kindle screensaver/suspend. The process and UiState survive, but the panel
        // does not show our framebuffer. Invalidate first so nothing below can act on
        // invisible UI; re-present the unchanged UiState once the proven wake event arrives.
        let display_ev = display_rx.take();
        router.on_display_events(display_ev);
        if display_ev.invalidated {
            touch_arb.cancel();
            pending_ui = None;
            nav.clear_on_stream_gap();
            let _ = eraser.clear_on_stream_gap();
            let wet = ctrl.session.is_wet_active() || ctrl.is_armed();
            if wet {
                // Abandon (never commit) an interrupted wet stroke; committed ink unchanged.
                if let Err(e) = ctrl.on_stream_gap(true) {
                    eprintln!("error: display invalidate stroke cancel: {e}");
                    let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                    shutdown_safe = safe;
                    stylus_mode_disarm_failed = disarm_fail || e.is_fatal();
                    break 1;
                }
            }
            ctrl.clear_pending_clean();
            println!(
                "display_invalidated reason=goingToScreenSaver view={:?} modal={} wet_stroke_abandoned={wet}",
                ui.view,
                ui.modal_open()
            );
            // Never leave a stale notebook/Home frame on the sleeping panel: paint the sleep
            // screen, then defend it against blanket's later screensaver paint (sleep guard).
            // Wake re-presents the unchanged UiState (F11.1 restore). Failures are cosmetic
            // and never fatal.
            sleep_guard = match ctrl.transport_mut().render_sleep() {
                Ok(fp) => Some(crate::sleep_ui::SleepGuard::new(fp, Instant::now())),
                Err(e) => {
                    eprintln!("warning: sleep screen: {e}");
                    None
                }
            };
        }
        if display_ev.restored {
            if let Some(g) = sleep_guard.take() {
                println!(
                    "sleep_screen_guard_end reason=restore reasserts={}",
                    g.reasserts
                );
            }
            println!("display_restore_requested reason=exitingScreenSaver");
        }
        if let Some(g) = sleep_guard.as_mut() {
            use crate::sleep_ui::GuardStep;
            match g.step(Instant::now()) {
                GuardStep::Wait => {}
                GuardStep::Done => {
                    println!(
                        "sleep_screen_guard_end reason=window reasserts={}",
                        g.reasserts
                    );
                    sleep_guard = None;
                }
                GuardStep::Check => {
                    if g.overwritten(ctrl.transport().fb_fingerprint()) {
                        match ctrl.transport_mut().render_sleep() {
                            Ok(fp) => {
                                g.reasserted(fp);
                                println!("sleep_screen_reasserted n={}", g.reasserts);
                            }
                            Err(e) => {
                                eprintln!("warning: sleep screen reassert: {e}");
                                sleep_guard = None;
                            }
                        }
                    }
                }
            }
        }

        // F11.3: the transport paints chrome only while UiState says it is visible.
        ctrl.transport_mut()
            .set_chrome_visible(ui.chrome_overlay_visible());

        // Eligible first: no trigger is consumed while nav/erase/wet ink is active. Library
        // triggers have deterministic priority over page-structure triggers, so at most one
        // structural operation runs per loop.
        let idle_disarmed =
            !nav.is_active() && !eraser.is_active() && ctrl.is_idle() && !ctrl.is_armed();
        // Structural/UI work also requires a valid (visible) display.
        let structure_eligible = idle_disarmed && router.display_valid();
        macro_rules! ui_exec {
            () => {
                UiExec {
                    ink_request: cfg.ink_request,
                    geo: g,
                    library: &mut library,
                    notebook_dir: &mut notebook_dir,
                    manifest: &mut manifest,
                    notebook_profile: &mut notebook_profile,
                    current_bg: &mut current_bg,
                    ui: &mut ui,
                    folders: &mut folders,
                    hosted: cfg.display_events_stdin,
                    stop: &stop,
                    import_run: &mut import_run,
                    prefs_path: &prefs_path,
                    sync_dir: cfg.sync_dir.as_deref(),
                    lasso: &mut lasso,
                    overlay: &mut overlay,
                    cached_move_ids: &mut cached_move_ids,
                    last_move_undo: &mut last_move_undo,
                }
            };
        }

        if idle_disarmed && router.redraw_requested() {
            let reason = if router.display_valid() {
                "requested"
            } else {
                "display_invalid"
            };
            match execute_ui_command(UiCommand::Redraw(Redraw::Full), &mut ui_exec!(), &mut ctrl) {
                Ok(()) => {
                    router.display_presented();
                    // Visible chrome gets a fresh 3 s countdown after wake; hidden stays hidden.
                    ui.chrome.display_restored(ms_since_start(Instant::now()));
                    println!(
                        "display_restored reason={reason} view={:?} modal={} home_selected={} home_page={} page={} chrome_visible={}",
                        ui.view,
                        ui.modal_open(),
                        ui.home.selected_index,
                        ui.home.page,
                        ctrl.session.page_id(),
                        ui.chrome_overlay_visible()
                    );
                }
                Err(e) => {
                    eprintln!("error: display restore present: {e}");
                    let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                    shutdown_safe = safe;
                    stylus_mode_disarm_failed = disarm_fail;
                    break 1;
                }
            }
            continue;
        }

        // F11: a pen tap already resolved into a command runs here, only while idle and
        // disarmed (never mid-stroke, never while stylus mode is armed).
        if structure_eligible {
            if let Some(cmd) = pending_ui.take() {
                let label = format!("{cmd:?}");
                if cmd != UiCommand::Undo {
                    lasso.clear();
                    overlay.clear();
                    cached_move_ids = None;
                    last_move_undo = None;
                }
                ctrl.transport_mut()
                    .set_chrome_visible(ui.chrome_overlay_visible());
                let was_modal = ui.modal_open();
                let result = execute_ui_command(cmd, &mut ui_exec!(), &mut ctrl);
                // An executor-raised error dialog suspends auto-hide like any modal.
                ui.note_modal_transition(was_modal, ms_since_start(Instant::now()));
                if let Err(e) = result {
                    eprintln!("error: ui command {label}: {e}");
                    let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                    shutdown_safe = safe;
                    stylus_mode_disarm_failed = disarm_fail;
                    break 1;
                }
                continue;
            }
        }

        // F11.3: auto-hide after 3 s without chrome interaction, and keep the panel's chrome
        // in sync with UiState by re-presenting from durable background + strokes (never by
        // painting white over the bands). Idle, disarmed, visible display, no modal only.
        if structure_eligible
            && pending_ui.is_none()
            && ui.allows_page_input()
            && matches!(lasso, LassoState::Idle)
        {
            let now_ms = ms_since_start(Instant::now());
            if ui.chrome.timeout_due(now_ms) {
                ui.chrome.hide();
                println!(
                    "chrome_auto_hide after_ms={}",
                    crate::fullscreen::CHROME_AUTO_HIDE_MS
                );
            }
            let want = ui.chrome_overlay_visible();
            if ctrl.transport().chrome_on_panel() != want {
                ctrl.transport_mut().set_chrome_visible(want);
                if let Err(e) =
                    execute_ui_command(UiCommand::Redraw(Redraw::Full), &mut ui_exec!(), &mut ctrl)
                {
                    eprintln!("error: chrome present: {e}");
                    let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                    shutdown_safe = safe;
                    stylus_mode_disarm_failed = disarm_fail;
                    break 1;
                }
                println!("chrome_presented visible={want}");
                continue;
            }
        }

        // D1: repaint the Sync screen when the server-link state changed (idle safe point,
        // no modal; the Sync view is never a writing surface).
        if structure_eligible
            && pending_ui.is_none()
            && ui.view == AppView::Sync
            && !ui.modal_open()
        {
            let latest = crate::server_link::with_global(|l| l.view().version);
            if latest.is_some() && latest != ui.sync_view.as_ref().map(|v| v.version) {
                if let Err(e) =
                    execute_ui_command(UiCommand::Redraw(Redraw::Full), &mut ui_exec!(), &mut ctrl)
                {
                    eprintln!("error: sync present: {e}");
                    let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                    shutdown_safe = safe;
                    stylus_mode_disarm_failed = disarm_fail;
                    break 1;
                }
                continue;
            }
        }
        // 1H-D2: a restore stopped sync (finished, cancelled after confirmation, or failed and
        // acknowledged): restart the session cleanly once its last state has been painted.
        if ui.view == AppView::Sync
            && !stop.load(Ordering::SeqCst)
            && crate::server_link::with_global(|l| l.view().restart) == Some(true)
            && ui.sync_view.as_ref().is_some_and(|v| v.restart)
        {
            println!("sync_restore=restart_requested");
            RESTART_REQUESTED.store(true, Ordering::SeqCst);
            stop.store(true, Ordering::SeqCst);
            continue;
        }

        // S2C inbound materialization: only at the same safe point as structural work, never
        // under a gesture, a pending command or an open modal, and never while the display is
        // invalid (the durable inbox just waits; the next valid-display loop applies it).
        if structure_eligible
            && pending_ui.is_none()
            && !ui.modal_open()
            && ui.view == AppView::Tools
            && import_run.is_some()
        {
            if let Err(e) = import_step(&mut ui_exec!(), &mut ctrl) {
                eprintln!("error: nbk import present: {e}");
                let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                shutdown_safe = safe;
                stylus_mode_disarm_failed = disarm_fail;
                break 1;
            }
            continue;
        }

        if structure_eligible
            && pending_ui.is_none()
            && !ui.modal_open()
            && import_run.is_none()
            && !matches!(
                lasso,
                LassoState::LassoDrawing { .. } | LassoState::Moving { .. }
            )
            && crate::input_lag::inbound_may_run(
                src.tool_in_proximity(),
                ctrl.transport().now_us().saturating_sub(last_stylus_us),
                src.backlog(),
            )
        {
            let t_apply = ctrl.transport().now_us();
            let applied = apply_inbound_at_safe_point(&mut ui_exec!(), &mut ctrl);
            if matches!(applied, Ok(true)) {
                // A real remote materialization invalidates replica-local object addresses.
                // The materializer's presentation has already replaced any transient overlay.
                lasso.clear();
                overlay.clear();
                cached_move_ids = None;
                last_move_undo = None;
                let end = ctrl.transport().now_us();
                let dur = end.saturating_sub(t_apply);
                inbound_apply_count += 1;
                inbound_apply_max_us = inbound_apply_max_us.max(dur);
                input_lag.blocked("inbound_apply", dur, end);
                println!("sync_inbound_main_thread_ms={}", dur / 1000);
            }
            match applied {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    eprintln!("error: {e}");
                    let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                    shutdown_safe = safe;
                    stylus_mode_disarm_failed = disarm_fail;
                    break 1;
                }
            }
        }

        // Developer-only triggers (diagnostics/tests). Not part of the product UI.
        let mut handled_library_op = false;
        if structure_eligible
            && !restore_flow_open(&ui)
            && !matches!(
                lasso,
                LassoState::LassoDrawing { .. } | LassoState::Moving { .. }
            )
        {
            if let Some(op) = peek_dev_library_op(&cfg.data_dir) {
                handled_library_op = true;
                consume_dev_library_op(&cfg.data_dir, op);
                let total = library.manifest().notebook_ids.len();
                let on_home = ui.view == AppView::Home && !ui.modal_open();
                let op_result: Result<(), String> = match op {
                    DevLibraryOp::Home => {
                        execute_ui_command(UiCommand::GoHome, &mut ui_exec!(), &mut ctrl)
                    }
                    DevLibraryOp::Next | DevLibraryOp::Previous if on_home => {
                        if op == DevLibraryOp::Next {
                            ui.home.select_next(total);
                        } else {
                            ui.home.select_previous(total);
                        }
                        println!(
                            "home_selection={} persisted=false",
                            library.manifest().notebook_ids[ui.home.selected_index]
                        );
                        execute_ui_command(
                            UiCommand::Redraw(Redraw::Full),
                            &mut ui_exec!(),
                            &mut ctrl,
                        )
                    }
                    DevLibraryOp::Open if on_home => {
                        let target =
                            library.manifest().notebook_ids[ui.home.selected_index].clone();
                        execute_ui_command(
                            UiCommand::OpenNotebook(target),
                            &mut ui_exec!(),
                            &mut ctrl,
                        )
                    }
                    DevLibraryOp::Create if on_home => {
                        // F10 developer semantics: create + select, stay on Home.
                        let paper = paper_background(&g);
                        match create_notebook_durable(
                            &mut library,
                            &notebook_profile,
                            &g,
                            &paper,
                            None,
                            &CreateNotebookHooks::default(),
                        ) {
                            Ok(commit) => {
                                sync_notebook_created(&library, &commit.notebook_id, &commit.title);
                                ui.home = HomeState::for_active(&library);
                                println!(
                                    "notebook_created notebook={} title={:?} manifest_persist_us={} active_notebook={} app_view=home",
                                    commit.notebook_id,
                                    commit.title,
                                    commit.manifest_persist_us,
                                    library.active_notebook_id()
                                );
                                execute_ui_command(
                                    UiCommand::Redraw(Redraw::Full),
                                    &mut ui_exec!(),
                                    &mut ctrl,
                                )
                                .map_err(|e| {
                                    format!(
                                        "FATAL: notebook create committed but Home redraw failed: {e}"
                                    )
                                })
                            }
                            Err(e) => Err(e),
                        }
                    }
                    DevLibraryOp::Delete if on_home => {
                        let target =
                            library.manifest().notebook_ids[ui.home.selected_index].clone();
                        execute_ui_command(
                            UiCommand::DeleteNotebook(target),
                            &mut ui_exec!(),
                            &mut ctrl,
                        )
                    }
                    other => {
                        println!(
                            "library_trigger_ignored op={other:?} reason=not_home_or_modal_open"
                        );
                        Ok(())
                    }
                };
                if let Err(e) = op_result {
                    eprintln!("error: library op {op:?}: {e}");
                    let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                    shutdown_safe = safe;
                    stylus_mode_disarm_failed = disarm_fail;
                    break 1;
                }
            }
        }

        if structure_eligible
            && !handled_library_op
            && ui.allows_page_input()
            && !matches!(
                lasso,
                LassoState::LassoDrawing { .. } | LassoState::Moving { .. }
            )
        {
            if let Some(op) = peek_dev_notebook_op(&cfg.data_dir) {
                consume_dev_notebook_op(&cfg.data_dir, op);
                let op_result = match op {
                    DevNotebookOp::MoveLeft => move_current_page(
                        &notebook_dir,
                        &mut manifest,
                        &mut ctrl,
                        &current_bg,
                        MovePageDirection::Left,
                    ),
                    DevNotebookOp::MoveRight => move_current_page(
                        &notebook_dir,
                        &mut manifest,
                        &mut ctrl,
                        &current_bg,
                        MovePageDirection::Right,
                    ),
                    DevNotebookOp::InsertAfter => insert_page_after_current(
                        &notebook_dir,
                        &mut manifest,
                        &mut ctrl,
                        &notebook_profile,
                        &mut current_bg,
                    ),
                    DevNotebookOp::DeleteCurrent => delete_current_page(
                        &notebook_dir,
                        &mut manifest,
                        &mut ctrl,
                        &notebook_profile,
                        &mut current_bg,
                    ),
                };
                match op_result {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("error: notebook structure op {op:?}: {e}");
                        let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                        shutdown_safe = safe;
                        stylus_mode_disarm_failed = disarm_fail;
                        break 1;
                    }
                }
            }
        }

        // F11 routing: modal UI > notebook chrome > Home UI > page gesture/ink engine.
        // A UI-owned contact (Down..Up) never reaches WetInk, navigation, Undo, or eraser.
        // F11.3 palm arbitration: sample tool proximity (the parser emits no event when the pen
        // leaves range). Suppression poisons any finger contact already down.
        let proximity = src.tool_in_proximity();
        touch_arb.on_proximity(proximity, ms_since_start(Instant::now()));
        if proximity.is_some() && proximity != logged_proximity {
            println!("stylus_proximity in_range={}", proximity == Some(true));
            logged_proximity = proximity;
        }
        let mut touch_ended = false;
        let mut touch_fatal: Option<String> = None;
        if let Some((pump, mapping)) = touch.as_ref() {
            while let Some((at, item)) = pump.try_next() {
                if touch_fatal.is_some() {
                    break;
                }
                let now_ms = ms_since_start(at);
                let frame_diag = TouchFrameDiag {
                    event_age_ms: at.elapsed().as_millis() as u64,
                    backlog_after: pump.backlog(),
                    produced_seq: pump.produced_seq(),
                    consumed_seq: pump.consumed_seq(),
                };
                match item {
                    TouchPoll::Frame(frame) => {
                        {
                            let dispatch_at = Instant::now();
                            let mut live = crate::launch_diag::live();
                            let (ui_x, ui_y) = match frame.contacts.first() {
                                Some(c) => match (c.x, c.y) {
                                    (Some(x), Some(y)) => mapping
                                        .map(x, y)
                                        .map(|p| (Some(p.x), Some(p.y)))
                                        .unwrap_or((None, None)),
                                    _ => (None, None),
                                },
                                None => (None, None),
                            };
                            live.emit_touch_frame(
                                at,
                                dispatch_at,
                                &frame,
                                pump.backlog(),
                                pump.produced_seq(),
                                pump.consumed_seq(),
                                ui_x,
                                ui_y,
                            );
                            live.maybe_emit_stats();
                        }
                        if let Some(action) = touch_arb.on_tick(now_ms, router.display_valid()) {
                            if let Some(p) = move_coalesce.take_pending_preview() {
                                if let Err(e) = present_selection_move_preview(
                                    &mut ctrl,
                                    &mut lasso,
                                    &mut overlay,
                                    &mut cached_move_ids,
                                    p,
                                ) {
                                    touch_fatal = Some(e);
                                    break;
                                }
                            }
                            if let Err(e) = handle_lasso_action(
                                action,
                                &mut ctrl,
                                &manifest,
                                &mut lasso,
                                &mut overlay,
                                &mut last_move_undo,
                                &mut cached_move_ids,
                                None,
                            ) {
                                touch_fatal = Some(e);
                                break;
                            }
                        }
                        let selection_bounds = current_selection_panel_bounds(&ctrl, &lasso);
                        let Some(action) = touch_arb.on_frame_with_selection(
                            &frame.contacts,
                            mapping,
                            now_ms,
                            router.display_valid(),
                            &ui,
                            &g,
                            selection_bounds,
                        ) else {
                            continue;
                        };
                        let decision = move_coalesce.ingest(action);
                        let action = match decision {
                            CoalesceDecision::Hold => {
                                // Latest-state paint is deferred, but LassoState must still
                                // track every Move so Finish/Cancel commit the real pointer.
                                if let Err(e) = handle_lasso_action(
                                    action,
                                    &mut ctrl,
                                    &manifest,
                                    &mut lasso,
                                    &mut overlay,
                                    &mut last_move_undo,
                                    &mut cached_move_ids,
                                    None,
                                ) {
                                    touch_fatal = Some(e);
                                    break;
                                }
                                continue;
                            }
                            CoalesceDecision::Supersede(action) => {
                                let skipped_preview = matches!(
                                    action,
                                    FingerAction::SelectionMoveFinished(_)
                                        | FingerAction::SelectionMoveCancelled
                                );
                                if matches!(action, FingerAction::SelectionMoveFinished(_)) {
                                    println!(
                                        "lasso_move_finished moves_seen={} coalesced={} previews_rendered={} preview_skipped_for_finish={} event_age_ms={} backlog_after={} produced_seq={} consumed_seq={}",
                                        move_coalesce.moves_seen,
                                        move_coalesce.coalesced,
                                        move_coalesce.previews_rendered,
                                        skipped_preview,
                                        frame_diag.event_age_ms,
                                        frame_diag.backlog_after,
                                        frame_diag.produced_seq,
                                        frame_diag.consumed_seq,
                                    );
                                } else {
                                    println!(
                                        "lasso_move_cancelled moves_seen={} coalesced={} previews_rendered={} event_age_ms={} backlog_after={}",
                                        move_coalesce.moves_seen,
                                        move_coalesce.coalesced,
                                        move_coalesce.previews_rendered,
                                        frame_diag.event_age_ms,
                                        frame_diag.backlog_after,
                                    );
                                }
                                action
                            }
                            CoalesceDecision::Handle(action) => {
                                if let Some(p) = move_coalesce.take_pending_preview() {
                                    if let Err(e) = present_selection_move_preview(
                                        &mut ctrl,
                                        &mut lasso,
                                        &mut overlay,
                                        &mut cached_move_ids,
                                        p,
                                    ) {
                                        touch_fatal = Some(e);
                                        break;
                                    }
                                    println!(
                                        "lasso_move_batch moves_seen={} previews_rendered={} coalesced={} finished=false event_age_ms={} backlog_after={}",
                                        move_coalesce.moves_seen,
                                        move_coalesce.previews_rendered,
                                        move_coalesce.coalesced,
                                        frame_diag.event_age_ms,
                                        frame_diag.backlog_after,
                                    );
                                }
                                action
                            }
                        };
                        match handle_lasso_action(
                            action,
                            &mut ctrl,
                            &manifest,
                            &mut lasso,
                            &mut overlay,
                            &mut last_move_undo,
                            &mut cached_move_ids,
                            Some(frame_diag),
                        ) {
                            Ok(true) => continue,
                            Ok(false) => {}
                            Err(e) => {
                                touch_fatal = Some(e);
                                break;
                            }
                        }
                        if pending_ui.is_some() {
                            println!("finger_action_dropped action={action:?} reason=busy");
                            continue;
                        }
                        if let FingerAction::Tap(p) = action {
                            if ui.view == AppView::Notebook && !ui.modal_open() {
                                if let Ok(xf) = ctrl.transport().profile.transform() {
                                    if let Some(page) = xf
                                        .viewport()
                                        .to_page(DisplayPoint::new(p.x as f32, p.y as f32))
                                    {
                                        if let Some(link) = ctrl.links().iter().rev().find(|l| {
                                            crate::link_paint::contains_point(l, page.x, page.y)
                                        }) {
                                            pending_ui = Some(UiCommand::OpenLinkPopup {
                                                link_id: link.link_id,
                                            });
                                            continue;
                                        }
                                    }
                                }
                            }
                        }
                        let was_visible = ui.chrome_overlay_visible();
                        let cmd = resolve_ui_input(
                            &mut ui,
                            &g,
                            UiInput::Finger(action),
                            &library,
                            &folders,
                            &manifest,
                            ctrl.session.page_id(),
                            now_ms,
                        );
                        println!(
                            "finger_action action={action:?} view={:?} modal={} command={cmd:?} chrome_visible={was_visible}->{}",
                            ui.view,
                            ui.modal_open(),
                            ui.chrome_overlay_visible()
                        );
                        let classify = crate::launch_diag::LaunchDiag::classify_for_action(
                            &mut touch_arb.tracker,
                            action,
                        );
                        crate::launch_diag::live().emit_finger_action(
                            session_start,
                            &format!("{action:?}"),
                            &format!("{:?}", ui.view),
                            ui.modal_open(),
                            &format!("{cmd:?}"),
                            classify,
                        );
                        if cmd != UiCommand::None {
                            pending_ui = Some(cmd);
                        }
                    }
                    TouchPoll::Gap => {
                        let _ = move_coalesce.discard_pending();
                        touch_arb.cancel();
                    }
                    TouchPoll::WouldBlock => {}
                    TouchPoll::EndOfStream => {
                        println!("touch=ended (continuing pen-only)");
                        touch_ended = true;
                        break;
                    }
                }
            }
        }
        if touch_fatal.is_none() {
            let tick_ms = ms_since_start(Instant::now());
            if let Some(action) = touch_arb.on_tick(tick_ms, router.display_valid()) {
                if let Err(e) = handle_lasso_action(
                    action,
                    &mut ctrl,
                    &manifest,
                    &mut lasso,
                    &mut overlay,
                    &mut last_move_undo,
                    &mut cached_move_ids,
                    None,
                ) {
                    touch_fatal = Some(e);
                }
            }
        }
        if touch_ended {
            touch = None;
        }
        if let Some(e) = touch_fatal {
            eprintln!("error: touch manipulation fatal: {e}");
            let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
            shutdown_safe = safe;
            stylus_mode_disarm_failed = disarm_fail;
            break 1;
        }

        let polled = src.poll_event();
        match &polled {
            Ok(SourcePoll::Event(ev)) => {
                last_stylus_us = ctrl.transport().now_us();
                if let Some(k) = ev.kernel_t_us() {
                    let now = last_stylus_us;
                    if let Some(stall) = input_lag.observe(k, now) {
                        println!("{}", stall.log_line(&format!("{:?}", ev.kind), now));
                    }
                }
                touch_arb.on_stylus_event(ev.kind, ev.in_proximity, ms_since_start(Instant::now()))
            }
            Ok(SourcePoll::StreamGap { .. }) => {
                touch_arb.on_stylus_gap(ms_since_start(Instant::now()))
            }
            _ => {}
        }
        if let Some(action) = touch_arb.take_cancelled_action() {
            let skipped = move_coalesce.discard_pending();
            let pen_hot = matches!(
                &polled,
                Ok(SourcePoll::Event(ev))
                    if matches!(ev.kind, StylusEventKind::Down | StylusEventKind::Move)
            );
            if pen_hot {
                // Pen safety and latency win. Cancel the touch mutation immediately, leave its
                // dirty pixels for the first idle regional cleanup, and do no GC16/layout work
                // on the pen dispatch path.
                lasso.clear();
                cached_move_ids = None;
                println!(
                    "touch_manipulation_cancelled reason=stylus cleanup=deferred preview_skipped={skipped}"
                );
            } else if let Err(e) = handle_lasso_action(
                action,
                &mut ctrl,
                &manifest,
                &mut lasso,
                &mut overlay,
                &mut last_move_undo,
                &mut cached_move_ids,
                None,
            ) {
                eprintln!("error: touch cancellation: {e}");
                let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                shutdown_safe = safe;
                stylus_mode_disarm_failed = disarm_fail;
                break 1;
            }
        }
        if let Some(p) = move_coalesce.take_pending_preview() {
            if let Err(e) = present_selection_move_preview(
                &mut ctrl,
                &mut lasso,
                &mut overlay,
                &mut cached_move_ids,
                p,
            ) {
                eprintln!("error: selection move preview: {e}");
                let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                shutdown_safe = safe;
                stylus_mode_disarm_failed = disarm_fail;
                break 1;
            }
            println!(
                "lasso_move_batch moves_seen={} previews_rendered={} coalesced={} finished=false",
                move_coalesce.moves_seen, move_coalesce.previews_rendered, move_coalesce.coalesced,
            );
        }
        if let Ok(SourcePoll::Event(ev)) = &polled {
            if ev.kind == StylusEventKind::Down {
                // Pen remains drawing-only. It also supersedes an old move in the narrow undo
                // slot, so the next Undo keeps its established newest-stroke meaning.
                let _ = move_coalesce.discard_pending();
                lasso.clear();
                cached_move_ids = None;
                last_move_undo = None;
            }
            let page_owned = nav.is_active() || eraser.is_active() || !ctrl.is_idle();
            let p = UiPoint::from_page(device_to_page(&cal, ev.device.x, ev.device.y));
            let consumed = match router.route_event(&ui, &g, ev.kind, ev.tool, p, page_owned) {
                DownRoute::Page => false,
                DownRoute::Consume => true,
                DownRoute::UiTap(p) => {
                    if pending_ui.is_none() {
                        let cmd = resolve_ui_input(
                            &mut ui,
                            &g,
                            UiInput::PenTap(p),
                            &library,
                            &folders,
                            &manifest,
                            ctrl.session.page_id(),
                            ms_since_start(Instant::now()),
                        );
                        println!(
                            "ui_tap x={} y={} view={:?} modal={} command={cmd:?}",
                            p.x,
                            p.y,
                            ui.view,
                            ui.modal_open()
                        );
                        if cmd != UiCommand::None {
                            pending_ui = Some(cmd);
                        }
                    }
                    true
                }
            };
            if consumed {
                continue;
            }
        }
        match polled {
            Ok(SourcePoll::Event(ev)) => match ev.kind {
                StylusEventKind::Hover => {}
                StylusEventKind::Down if ev.tool == StylusTool::Eraser => {
                    if nav.is_active() || eraser.is_active() {
                        continue;
                    }
                    if !ctrl.is_idle() || ctrl.is_armed() {
                        // Never interrupt an active pen stroke / armed wet path.
                        continue;
                    }
                    let page = device_to_page(&cal, ev.device.x, ev.device.y);
                    let strokes = ctrl.session.committed_strokes().to_vec();
                    let roots = ctrl.session.root_of_map().clone();
                    if !eraser.on_down(page, &strokes, &roots) {
                        continue;
                    }
                    ctrl.totals.eraser_gestures += 1;
                    // Provisional only — clip against stationary first sample.
                    eraser.process_to(page);
                    ctrl.totals.eraser_sweep_segments += 1;
                }
                StylusEventKind::Down if ev.tool == StylusTool::Pen => {
                    if nav.is_active() || eraser.is_active() {
                        continue;
                    }
                    if !ctrl.is_idle() {
                        continue;
                    }
                    // F11.3: pen Down in page content dismisses visible chrome and then *this
                    // same contact* proceeds to ink. The page is reconstructed from durable
                    // state (GC16 FULL, waited) before the wet stroke arms, so the reconstruct
                    // can never erase the beginning of the stroke; queued Moves replay after.
                    if !ctrl.is_armed() && ui.chrome.pen_content_down() {
                        ctrl.transport_mut()
                            .set_chrome_visible(ui.chrome_overlay_visible());
                        let strokes = ctrl.session.committed_strokes().to_vec();
                        let presented = chrome_for_page(&manifest, ctrl.session.page_id())
                            .and_then(|chrome| {
                                ctrl.transport_mut().reconstruct_page(
                                    &current_bg,
                                    &strokes,
                                    Some(chrome),
                                )
                            });
                        if let Err(e) = presented {
                            eprintln!("error: pen chrome dismiss present: {e}");
                            let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                            shutdown_safe = safe;
                            stylus_mode_disarm_failed = disarm_fail;
                            break 1;
                        }
                        ctrl.clear_pending_clean();
                        println!("chrome_hidden reason=pen_content_down");
                    }
                    let page = device_to_page(&cal, ev.device.x, ev.device.y);
                    if !ev.buttons.primary
                        && ui.view == AppView::Notebook
                        && !ui.modal_open()
                        && ctrl.is_idle()
                    {
                        if let Some(link) = ctrl
                            .links()
                            .iter()
                            .rev()
                            .find(|l| crate::link_paint::contains_point(l, page.x, page.y))
                        {
                            let id = link.link_id;
                            let _ = nav.on_down(NavDownAction::IgnoreNoInk);
                            pending_ui = Some(UiCommand::OpenLinkPopup { link_id: id });
                            continue;
                        }
                    }
                    if let Some(dispatch) =
                        dispatch_side_button(ui.side_button, ev.buttons.primary, page)
                    {
                        match dispatch {
                            SideButtonDispatch::CreateLink { page } => {
                                println!("create_link_gesture x={:.0} y={:.0}", page.x, page.y);
                                let _ = nav.on_down(NavDownAction::IgnoreNoInk);
                                pending_ui = Some(UiCommand::BeginCreateLink {
                                    x: page.x,
                                    y: page.y,
                                });
                                continue;
                            }
                            SideButtonDispatch::Nav(action) => {
                                let zone = crate::nav::classify_nav_zone(page);
                                println!(
                            "nav_gesture side=true zone={zone:?} x={:.0} y={:.0} action={action:?}",
                            page.x, page.y
                        );
                                let act = nav.on_down(action);
                                match act {
                                    NavDownAction::IgnoreNoInk => {
                                        // Consume contact until Up: no ink, undo, switch, or persist.
                                    }
                                    NavDownAction::UndoLastStroke => {
                                        ctrl.totals.undo_gestures += 1;
                                        if !ctrl.is_idle() || ctrl.is_armed() {
                                            // Never interrupt wet stroke / armed stylus mode.
                                            continue;
                                        }
                                        if let Err(e) = undo_and_present(
                                            &mut ctrl,
                                            &manifest,
                                            &current_bg,
                                            &mut lasso,
                                            &mut overlay,
                                            &mut last_move_undo,
                                            &mut cached_move_ids,
                                        ) {
                                            eprintln!("{e}");
                                            let (_c, safe, disarm_fail) =
                                                finish_shutdown(&mut ctrl, false);
                                            shutdown_safe = safe;
                                            stylus_mode_disarm_failed = disarm_fail;
                                            break 1;
                                        }
                                    }
                                    NavDownAction::Prev | NavDownAction::NextOrCreate => {
                                        match navigate(
                                            &notebook_dir,
                                            &mut manifest,
                                            &mut ctrl,
                                            &notebook_profile,
                                            &mut current_bg,
                                            act,
                                        ) {
                                            Ok(_) => {}
                                            Err(e) => {
                                                eprintln!("error: page navigation: {e}");
                                                let (_c, safe, disarm_fail) =
                                                    finish_shutdown(&mut ctrl, false);
                                                shutdown_safe = safe;
                                                stylus_mode_disarm_failed = disarm_fail;
                                                break 1;
                                            }
                                        }
                                    }
                                }
                                continue;
                            }
                        }
                    }
                    match ctrl.on_pen_down(page, ev.pressure) {
                        Ok(id) => {
                            let dispatch_us = Instant::now();
                            let kernel_us = ev.kernel_t_us();
                            let since_up_k = match (kernel_us, ctrl.last_up_kernel_us) {
                                (Some(k), Some(u)) => Some(k.saturating_sub(u)),
                                _ => None,
                            };
                            let since_up_h = ctrl.last_up_handler_return_us.map(|u| {
                                // Approximate: wall Instant vs transport clock — use transport now.
                                ctrl.transport().now_us().saturating_sub(u)
                            });
                            let rapid = since_up_h.map(|d| d < 500_000).unwrap_or(false)
                                || since_up_k.map(|d| d < 500_000).unwrap_or(false);
                            if rapid {
                                ctrl.totals.rapid_second_down_count += 1;
                                ctrl.totals.rapid_second_down_accepted += 1;
                                if let Some(d) = since_up_h {
                                    ctrl.totals.rapid_second_down_dispatch_max_us =
                                        ctrl.totals.rapid_second_down_dispatch_max_us.max(d);
                                }
                            }
                            if cfg.trace_interstroke {
                                let _ = dispatch_us;
                                println!(
                                    "trace Down accepted=true tool={:?} phase={:?} armed={} prev_stroke={:?} \
                                     kernel_t_us={:?} dispatch_delay_us={:?} time_since_previous_up_kernel_us={:?} \
                                     time_since_previous_up_handler_return_us={:?} rapid={}",
                                    ev.tool,
                                    ctrl.session.phase(),
                                    ctrl.is_armed(),
                                    ctrl.last_finished_stroke_id,
                                    kernel_us,
                                    None::<u64>,
                                    since_up_k,
                                    since_up_h,
                                    rapid
                                );
                            }
                            println!("stroke_started id={id}");
                        }
                        Err(e) => {
                            let kernel_us = ev.kernel_t_us();
                            let since_up_h = ctrl
                                .last_up_handler_return_us
                                .map(|u| ctrl.transport().now_us().saturating_sub(u));
                            let rapid = since_up_h.map(|d| d < 500_000).unwrap_or(false);
                            if rapid {
                                ctrl.totals.rapid_second_down_count += 1;
                                ctrl.totals.rapid_second_down_rejected += 1;
                                if let Some(d) = since_up_h {
                                    ctrl.totals.rapid_second_down_dispatch_max_us =
                                        ctrl.totals.rapid_second_down_dispatch_max_us.max(d);
                                }
                            }
                            if cfg.trace_interstroke {
                                println!(
                                    "trace Down accepted=false reason={e} tool={:?} phase={:?} armed={} \
                                     prev_stroke={:?} kernel_t_us={:?} time_since_previous_up_handler_return_us={:?} rapid={}",
                                    ev.tool,
                                    ctrl.session.phase(),
                                    ctrl.is_armed(),
                                    ctrl.last_finished_stroke_id,
                                    kernel_us,
                                    since_up_h,
                                    rapid
                                );
                            }
                            eprintln!("error: pen down: {e}");
                            if e.is_fatal() {
                                let (c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                                shutdown_safe = safe;
                                stylus_mode_disarm_failed = disarm_fail;
                                break if c != 0 { c } else { 1 };
                            }
                        }
                    }
                }
                StylusEventKind::Down => {}
                StylusEventKind::Move => {
                    if nav.is_active() {
                        continue;
                    }
                    if eraser.is_active() {
                        // Provisional clip only — no persist / reconstruct until Up.
                        let page = device_to_page(&cal, ev.device.x, ev.device.y);
                        eraser.process_to(page);
                        ctrl.totals.eraser_sweep_segments += 1;
                        continue;
                    }
                    if !ctrl.session.is_wet_active() {
                        continue;
                    }
                    if ev.tool != StylusTool::Pen {
                        continue;
                    }
                    let page = device_to_page(&cal, ev.device.x, ev.device.y);
                    let kin = input_kernel_for_du_timing(input_clock, ev.kernel_t_us());
                    if let Err(e) = ctrl.on_pen_move(page, ev.pressure, kin) {
                        eprintln!("error: pen move: {e}");
                        match ctrl.on_stream_gap(true) {
                            Ok(()) => println!("stroke_cancelled_recoverable=true (pen move)"),
                            Err(fe) => {
                                eprintln!("error: cancellation after pen move: {fe}");
                                let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                                shutdown_safe = safe;
                                stylus_mode_disarm_failed = disarm_fail || fe.is_fatal();
                                break 1;
                            }
                        }
                    }
                }
                StylusEventKind::Up => {
                    if eraser.is_active() {
                        let erase_sync = SyncedErase::begin(
                            &eraser,
                            &manifest.notebook_id,
                            ctrl.session.page_id(),
                        );
                        let commit = erase_sync.on_up(&mut eraser);
                        let erase_chrome = match chrome_for_page(&manifest, ctrl.session.page_id())
                        {
                            Ok(l) => l,
                            Err(e) => {
                                eprintln!("partial_erase fatal=true label: {e}");
                                let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                                shutdown_safe = safe;
                                stylus_mode_disarm_failed = disarm_fail;
                                break 1;
                            }
                        };
                        let ticket = match erase_sync.prepare(&commit) {
                            Ok(t) => t,
                            Err(e) => {
                                eprintln!("erase_capture_failed detail={e}");
                                continue;
                            }
                        };
                        let erase_result =
                            eraser_commit_gesture(&mut ctrl, &current_bg, erase_chrome, &commit);
                        // Durable replace done ⇔ the persist step succeeded (a later presentation
                        // failure is fatal but the edit is authoritative). A persist failure
                        // leaves the prep unresolved; startup reconciliation settles it.
                        if erase_result.is_ok()
                            || erase_result
                                .as_ref()
                                .is_err_and(|e| e.contains("durable edit kept"))
                        {
                            if let Some(t) = ticket {
                                crate::sync_outbound::with_global(|s| s.commit(t));
                            }
                        }
                        if let Err(e) = erase_result {
                            eprintln!("{e}");
                            let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                            shutdown_safe = safe;
                            stylus_mode_disarm_failed = disarm_fail;
                            break 1;
                        }
                        continue;
                    }
                    if nav.is_active() {
                        nav.on_up();
                        continue;
                    }
                    if !ctrl.session.is_wet_active() {
                        continue;
                    }
                    let page = device_to_page(&cal, ev.device.x, ev.device.y);
                    if cfg.trace_interstroke {
                        println!(
                            "trace Up tool={:?} phase={:?} armed={} kernel_t_us={:?}",
                            ev.tool,
                            ctrl.session.phase(),
                            ctrl.is_armed(),
                            ev.kernel_t_us()
                        );
                    }
                    match ctrl.on_pen_up(page, ev.pressure) {
                        Ok(committed_id) => {
                            ctrl.last_up_kernel_us = ev.kernel_t_us();
                            if let Some(stroke) = ctrl
                                .session
                                .committed_strokes()
                                .iter()
                                .rev()
                                .find(|s| s.id == committed_id)
                            {
                                let t_cap = ctrl.transport().now_us();
                                crate::sync_outbound::with_global(|s| {
                                    s.stroke_committed(
                                        &manifest.notebook_id,
                                        ctrl.session.page_id(),
                                        stroke,
                                    )
                                });
                                let end = ctrl.transport().now_us();
                                let cap_us = end.saturating_sub(t_cap);
                                sync_capture_max_us = sync_capture_max_us.max(cap_us);
                                if cap_us >= 5_000 {
                                    input_lag.blocked("sync_capture", cap_us, end);
                                    println!("sync_capture_slow us={cap_us}");
                                }
                            }
                            if cfg.trace_pressure {
                                if let Some(stroke) = ctrl.session.committed_strokes().last() {
                                    let summary = PressureStrokeSummary::from_stroke(stroke);
                                    println!("{}", summary.log_line(stroke.id.as_str()));
                                }
                            }
                        }
                        Err(CtrlError::Recoverable(m)) => {
                            eprintln!("stroke aborted (recoverable): {m}");
                            ctrl.last_up_kernel_us = ev.kernel_t_us();
                        }
                        Err(CtrlError::Fatal(m)) => {
                            eprintln!("error: pen up fatal: {m}");
                            let (c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                            shutdown_safe = safe;
                            stylus_mode_disarm_failed = disarm_fail;
                            break if c != 0 { c } else { 1 };
                        }
                    }
                }
            },
            Ok(SourcePoll::StreamGap { had_active_contact }) => {
                router.on_stream_gap();
                if !ui.allows_page_input() {
                    nav.clear_on_stream_gap();
                    let _ = eraser.clear_on_stream_gap();
                    continue;
                }
                if cfg.trace_interstroke {
                    println!(
                        "trace StreamGap had_active_contact={had_active_contact} phase={:?} armed={} post_up={}",
                        ctrl.session.phase(),
                        ctrl.is_armed(),
                        ctrl.last_up_handler_return_us.is_some() && ctrl.is_idle()
                    );
                }
                if ctrl.is_idle() && ctrl.last_up_handler_return_us.is_some() {
                    ctrl.totals.input_stream_gaps_during_post_up += 1;
                }
                nav.clear_on_stream_gap();
                // Discard provisional eraser — committed model/panel unchanged.
                let _ = eraser.clear_on_stream_gap();
                match ctrl.on_stream_gap(had_active_contact) {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("error: stream gap fatal: {e}");
                        let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                        shutdown_safe = safe;
                        stylus_mode_disarm_failed = disarm_fail || e.is_fatal();
                        break 1;
                    }
                }
            }
            Ok(SourcePoll::WouldBlock) => {
                ctrl.totals.max_input_backlog =
                    ctrl.totals.max_input_backlog.max(src.backlog() as u64);
                // No partial refresh may land on a panel the screensaver owns.
                let idle_clean = if router.display_valid() {
                    ctrl.maybe_idle_clean(src.backlog() == 0)
                } else {
                    Ok(false)
                };
                if let Err(e) = idle_clean {
                    eprintln!("error: idle clean: {e}");
                    let (_c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                    shutdown_safe = safe;
                    stylus_mode_disarm_failed = disarm_fail;
                    break 1;
                }
                if router.display_valid()
                    && ctrl.is_idle()
                    && !ctrl.is_armed()
                    && !ctrl.has_pending_clean()
                    && matches!(lasso, LassoState::Idle)
                    && overlay.is_some()
                {
                    let dirty = overlay.take_cleanup();
                    let (transport, strokes) = ctrl.transport_and_strokes();
                    if let Err(e) = transport.clear_touch_overlay(strokes, dirty) {
                        eprintln!("warning: deferred touch-overlay cleanup failed: {e}");
                    }
                }
                // Live ink that crossed a chrome band: repaint chrome once the deferred GC16
                // has flushed (idle + disarmed only). Presentation only; never persisted.
                if ui.allows_page_input()
                    && router.display_valid()
                    && ctrl.transport().chrome_damaged()
                    && ctrl.is_idle()
                    && !ctrl.is_armed()
                    && !ctrl.has_pending_clean()
                    && !nav.is_active()
                    && !eraser.is_active()
                    && src.backlog() == 0
                {
                    match chrome_for_page(&manifest, ctrl.session.page_id()) {
                        Ok(chrome) => {
                            if let Err(e) = ctrl.transport_mut().repair_chrome(chrome) {
                                eprintln!("warning: chrome repair failed: {e}");
                            }
                        }
                        Err(e) => eprintln!("warning: chrome repair skipped: {e}"),
                    }
                }
                if stop.load(Ordering::SeqCst) {
                    println!("shutdown=signal");
                    let (c, safe, disarm_fail) = finish_shutdown(&mut ctrl, true);
                    shutdown_safe = safe;
                    stylus_mode_disarm_failed = disarm_fail;
                    break c;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Ok(SourcePoll::EndOfStream) => {
                eprintln!("error: stylus EOF");
                let (c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                shutdown_safe = safe;
                stylus_mode_disarm_failed = disarm_fail;
                break if c != 0 { c } else { 1 };
            }
            Err(e) => {
                eprintln!("error: poll: {e}");
                let (c, safe, disarm_fail) = finish_shutdown(&mut ctrl, false);
                shutdown_safe = safe;
                stylus_mode_disarm_failed = disarm_fail;
                break if c != 0 { c } else { 1 };
            }
        }
    };

    let session_duration = session_start.elapsed();
    let final_active = ctrl.session.page_id().to_string();
    let pump_max_backlog = src.max_backlog() as u64;
    drop(src);
    crate::launch_diag::live().emit_stats_at_shutdown();
    println!("eviocgrab=released");

    println!("# SCRIBE APP GATE F11 session summary");
    println!("session_duration_ms={}", session_duration.as_millis());
    println!(
        "library_notebooks={}",
        library.manifest().notebook_ids.len()
    );
    println!("active_notebook_id={}", library.active_notebook_id());
    println!(
        "final_app_view={}",
        match ui.view {
            AppView::Notebook => "notebook",
            AppView::Home => "home",
            AppView::Sync => "sync",
            AppView::Tools => "tools",
            AppView::Preferences => "preferences",
            AppView::LinkTargetPicker => "link_picker",
        }
    );
    println!("notebook_pages={}", manifest.page_ids.len());
    println!("initial_active_page={initial_active_page}");
    println!("final_active_page={final_active}");
    println!("page_switches={}", ctrl.totals.page_switches);
    println!("pages_created={}", ctrl.totals.pages_created);
    println!("pages_deleted={}", ctrl.totals.pages_deleted);
    println!("page_delete_refusals={}", ctrl.totals.page_delete_refusals);
    println!("page_delete_failures={}", ctrl.totals.page_delete_failures);
    println!("page_switch_failures={}", ctrl.totals.page_switch_failures);
    println!(
        "navigation_gestures_ignored={}",
        ctrl.totals.navigation_gestures_ignored
    );
    println!("eraser_gestures={}", ctrl.totals.eraser_gestures);
    println!("eraser_hits={}", ctrl.totals.eraser_hits);
    println!("eraser_misses={}", ctrl.totals.eraser_misses);
    println!(
        "eraser_sweep_segments={}",
        ctrl.totals.eraser_sweep_segments
    );
    println!(
        "eraser_unique_strokes_deleted={}",
        ctrl.totals.eraser_unique_strokes_deleted
    );
    println!(
        "eraser_duplicate_hits_suppressed={}",
        ctrl.totals.eraser_duplicate_hits_suppressed
    );
    println!(
        "eraser_blank_gestures={}",
        ctrl.totals.eraser_blank_gestures
    );
    println!("eraser_reconstructs={}", ctrl.totals.eraser_reconstructs);
    println!("strokes_deleted={}", ctrl.totals.strokes_deleted);
    println!(
        "delete_persistence_errors={}",
        ctrl.totals.delete_persistence_errors
    );
    println!(
        "eraser_reconstruct_failures={}",
        ctrl.totals.eraser_reconstruct_failures
    );
    println!(
        "partial_erase_gestures={}",
        ctrl.totals.partial_erase_gestures
    );
    println!(
        "partial_erase_sources_touched={}",
        ctrl.totals.partial_erase_sources_touched
    );
    println!(
        "partial_erase_full_deletes={}",
        ctrl.totals.partial_erase_full_deletes
    );
    println!(
        "partial_erase_fragments_created={}",
        ctrl.totals.partial_erase_fragments_created
    );
    println!(
        "partial_erase_empty_gestures={}",
        ctrl.totals.partial_erase_empty_gestures
    );
    println!(
        "partial_erase_persistence_errors={}",
        ctrl.totals.partial_erase_persistence_errors
    );
    println!(
        "partial_erase_reconstruct_failures={}",
        ctrl.totals.partial_erase_reconstruct_failures
    );
    println!("undo_gestures={}", ctrl.totals.undo_gestures);
    println!("undo_hits={}", ctrl.totals.undo_hits);
    println!("undo_empty={}", ctrl.totals.undo_empty);
    println!(
        "undo_persistence_errors={}",
        ctrl.totals.undo_persistence_errors
    );
    println!(
        "undo_reconstruct_failures={}",
        ctrl.totals.undo_reconstruct_failures
    );
    println!(
        "page_open={}",
        if ctrl.store().is_bound() {
            "bound"
        } else {
            "deferred"
        }
    );
    println!("persisted_render_ink={:02x}", notebook_profile.ink_byte);
    println!(
        "persisted_strokes_loaded={}",
        ctrl.session.committed_count()
    );
    println!("reconstructed_strokes={}", ctrl.session.committed_count());
    println!("strokes_started={}", ctrl.session.strokes_started());
    println!("strokes_committed={}", ctrl.session.strokes_committed());
    println!("strokes_cancelled={}", ctrl.session.strokes_cancelled());
    println!("strokes_durable={}", ctrl.totals.strokes_durable);
    println!("persistence_errors={}", ctrl.totals.persistence_errors);
    println!("input_stream_gaps={}", ctrl.totals.input_stream_gaps);
    println!("idle_stream_gaps={}", ctrl.totals.input_stream_gaps_idle);
    println!(
        "active_stream_gaps={}",
        ctrl.totals.input_stream_gaps_active
    );
    println!("DU sends={}", ctrl.totals.du_sends);
    println!("DU send errors={}", ctrl.totals.du_send_errors);
    println!(
        "GC16 commit refreshes={}",
        ctrl.totals.gc16_commit_refreshes
    );
    println!("idle_clean_refreshes={}", ctrl.totals.idle_clean_refreshes);
    println!(
        "rapid_second_down_count={}",
        ctrl.totals.rapid_second_down_count
    );
    println!(
        "rapid_second_down_accepted={}",
        ctrl.totals.rapid_second_down_accepted
    );
    println!(
        "rapid_second_down_rejected={}",
        ctrl.totals.rapid_second_down_rejected
    );
    println!(
        "rapid_second_down_dispatch_max_us={}",
        ctrl.totals.rapid_second_down_dispatch_max_us
    );
    println!(
        "input_stream_gaps_during_post_up={}",
        ctrl.totals.input_stream_gaps_during_post_up
    );
    println!(
        "max_input_backlog={}",
        ctrl.totals.max_input_backlog.max(pump_max_backlog)
    );
    println!(
        "input_lag max_dispatch_delay_ms={} stalls_ge_{}ms={} max_block={:?}",
        input_lag.max_delay_us / 1000,
        crate::input_lag::STALL_US / 1000,
        input_lag.stalls,
        input_lag.max_block
    );
    println!(
        "sync_main_thread capture_max_us={sync_capture_max_us} inbound_applies={inbound_apply_count} inbound_apply_max_ms={}",
        inbound_apply_max_us / 1000
    );
    println!("final page stroke count={}", ctrl.session.committed_count());
    println!("store stroke count={}", ctrl.store().stroke_count());
    println!("EVIOCGRAB released=true");
    println!("shutdown_safe={shutdown_safe}");
    println!("stylus_mode_disarm_failed={stylus_mode_disarm_failed}");
    println!("note: timings are software/ioctl intervals, not physical pen→panel latency");
    if !shutdown_safe || stylus_mode_disarm_failed || ctrl.totals.persistence_errors > 0 {
        1
    } else {
        code
    }
}

/// Stable udev symlink to the `pt_mt` touchscreen (event4 on FW 5.18.6).
pub const DEFAULT_TOUCH_PATH: &str = "/dev/input/touch";

/// UI input already classified by the pen router or the finger gesture tracker.
#[derive(Clone, Copy, Debug)]
enum UiInput {
    PenTap(UiPoint),
    Finger(FingerAction),
}

/// Resolve UI input (pen or finger) through the single shared hit-testing path, including the
/// F11.3 chrome visibility bookkeeping (monotonic `now_ms`).
#[allow(clippy::too_many_arguments)]
fn resolve_ui_input(
    ui: &mut UiState,
    g: &FbGeometry,
    input: UiInput,
    library: &LibraryRepository,
    folders: &FolderStore,
    manifest: &PersistedNotebookManifestV1,
    page_id: &str,
    now_ms: u64,
) -> UiCommand {
    let cards = home_cards(library);
    refresh_folder_view(ui, folders, library);
    let notebook_ctx = if ui.view == AppView::Notebook {
        chrome_for_page(manifest, page_id)
            .ok()
            .map(|page| NotebookUiContext { page })
    } else {
        None
    };
    let title = library.next_default_title();
    match input {
        UiInput::PenTap(p) => ui.on_ui_tap(g, p, &cards, &title, notebook_ctx, now_ms),
        UiInput::Finger(a) => ui.on_finger(g, a, &cards, &title, notebook_ctx, now_ms),
    }
}

pub fn default_config(
    du_interval_us: u64,
    du_override: bool,
    args: &[String],
) -> Result<RuntimeConfig, String> {
    let fb_path = args
        .windows(2)
        .find(|w| w[0] == "--fb")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "/dev/fb0".into());
    let stylus_path = args
        .windows(2)
        .find(|w| w[0] == "--input")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| PREFERRED_STYLUS_PATH.into());
    let ink_request = parse_ink_request(args)?;
    let confirm_write = args.iter().any(|a| a == "--confirm-write");
    let trace_interstroke = args.iter().any(|a| a == "--trace-interstroke");
    let trace_pressure = args.iter().any(|a| a == "--trace-pressure");
    let display_events_stdin = args.iter().any(|a| a == "--display-events-stdin");
    let touch_path = if args.iter().any(|a| a == "--no-touch") {
        None
    } else {
        Some(
            args.windows(2)
                .find(|w| w[0] == "--touch")
                .map(|w| w[1].clone())
                .unwrap_or_else(|| DEFAULT_TOUCH_PATH.into()),
        )
    };
    let data_dir = resolve_data_dir(args);
    let sync_dir = args
        .windows(2)
        .find(|w| w[0] == "--sync-dir")
        .map(|w| PathBuf::from(&w[1]));
    let device_dir = args
        .windows(2)
        .find(|w| w[0] == "--device-dir")
        .map(|w| PathBuf::from(&w[1]));
    Ok(RuntimeConfig {
        fb_path,
        stylus_path,
        ink_request,
        du_interval_us,
        du_override,
        confirm_write,
        data_dir,
        trace_interstroke,
        trace_pressure,
        display_events_stdin,
        touch_path,
        sync_dir,
        device_dir,
    })
}
