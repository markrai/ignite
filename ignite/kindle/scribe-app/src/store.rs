//! Gate F1/F1.1/F3 durable page store: versioned meta + background.raw + strokes.jsonl + edits.jsonl
//! + texts.jsonl (synced `SyncText`; painted display-only from the current-page overlay).
//!
//! Page format v2 seals a render profile in `page.meta`.
//! Page format v3 adds an append-only `edits.jsonl` tombstone journal (`edit_journal_version`).
//! Stroke journal records remain independently versioned (`STROKE_RECORD_VERSION`) and do not
//! embed ink/brush. Deletes never rewrite `strokes.jsonl`.
//!
//! ## Background immutability
//!
//! After a page is successfully created, imported, or materialized, `background.raw` is
//! immutable except through [`install_background`] (today: chrome-scrub repair). Scribe is the
//! only writer. Every legitimate replacement clears any persisted sync identity, writes the new
//! bytes, then publishes checksum + identity in the same `page.meta` transaction. Sync snapshot
//! and inbound verify therefore reconstruct a background's SHA-256 from `page.meta` without
//! rereading the multi-megabyte file. Unexpected mutation (size mismatch, checksum mismatch on
//! reconstruction, malformed identity) fails closed or reconstructs once — never trusts `mtime`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kindle_platform::{rect_checksum, FbGeometry};
use notebook_core::{Point, Stroke, StrokeId, CANONICAL_HEIGHT, CANONICAL_WIDTH};
use serde::{Deserialize, Serialize};
use sync_protocol::{sha256_hex, BackgroundEncoding, BackgroundFormat, BackgroundRef, Sha256Hex};

use crate::pressure_curve::{STYLE_LEGACY_FIXED_V1, STYLE_PRESSURE_WIDTH_V1};
use crate::profile::{
    validate_framebuffer_vs_page, validate_persisted_render_profile_v1,
    validate_persisted_render_profile_v2, PersistedRenderProfileV1, PersistedRenderProfileV2,
    RenderProfile, LEGACY_FIXED_RADIUS, RENDER_PROFILE_VERSION, RENDER_PROFILE_VERSION_V1,
};

/// Page metadata / storage layout version (v5 = partial-erase edit journal).
pub const FORMAT_VERSION: u32 = 5;
/// Prior page format (pressure-capable render profile; edit journal v1 deletes only).
pub const FORMAT_VERSION_V4: u32 = 4;
/// Prior page format (edit journal; fixed-width render profile v1).
pub const FORMAT_VERSION_V3: u32 = 3;
/// Prior page format (render profile sealed; no edit journal).
pub const FORMAT_VERSION_V2: u32 = 2;
/// Legacy stroke JSONL record schema (fixed-width; no style tag).
pub const STROKE_RECORD_VERSION: u32 = 1;
/// F6 stroke JSONL record schema (carries `style`).
pub const STROKE_RECORD_VERSION_V2: u32 = 2;
/// Edit record schema: v1 `delete_stroke`.
pub const EDIT_RECORD_VERSION_V1: u32 = 1;
/// Edit record schema: v2 `replace_strokes`.
pub const EDIT_RECORD_VERSION_V2: u32 = 2;
/// Page.meta edit-journal schema version (v2 = deletes + replace_strokes).
pub const EDIT_JOURNAL_VERSION: u32 = 2;
/// Historical alias: pre-F7 pages sealed edit_journal_version = 1.
pub const EDIT_JOURNAL_VERSION_V1: u32 = 1;
/// Back-compat alias used by older call sites / tests for delete record v.
#[allow(dead_code)]
pub const EDIT_RECORD_VERSION: u32 = EDIT_RECORD_VERSION_V1;
pub const BG_LAYOUT_VERSION: u32 = 1;
pub const DEFAULT_DATA_DIR: &str = "/mnt/us/scribe-data";
pub const DEFAULT_PAGE_ID: &str = "page-1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedPageHeader {
    pub v: u32,
    pub page_id: String,
    pub canonical_width: f32,
    pub canonical_height: f32,
    pub panel_width: u32,
    pub panel_height: u32,
    pub line_length: u32,
    pub background_bytes: u64,
    pub background_checksum: u32,
    pub bg_layout_version: u32,
    /// Sealed render profile. Format ≤3 holds [`PersistedRenderProfileV1`]; format 4 holds
    /// [`PersistedRenderProfileV2`]. Stored as raw JSON; load dispatches on
    /// `render.profile_version` (never heuristic try-v2-then-v1).
    pub render: serde_json::Value,
    /// Present for format v3+; records edit-journal schema version.
    #[serde(default)]
    pub edit_journal_version: u32,
    /// Sync identity of the durable `background.raw` (SHA-256 or blank), published only after
    /// those bytes are fsynced. Optional so existing pages migrate in place without a format bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_identity: Option<BackgroundIdentityV1>,
}

/// Version of [`BackgroundIdentityV1`]. Unknown versions are reconstructed, never trusted.
pub const BACKGROUND_IDENTITY_VERSION: u32 = 1;

/// Durable content identity of one page background. Lives in `page.meta` next to
/// `background_checksum` / `background_bytes`. Valid only for the `background.raw` that was
/// written before this record was published.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackgroundIdentityV1 {
    Blank { v: u32 },
    Blob { v: u32, sha256: String },
}

impl BackgroundIdentityV1 {
    pub fn from_pixels(pixels: &[u8]) -> Self {
        if pixels.iter().all(|&b| b == 0xFF) {
            Self::Blank {
                v: BACKGROUND_IDENTITY_VERSION,
            }
        } else {
            Self::Blob {
                v: BACKGROUND_IDENTITY_VERSION,
                sha256: sha256_hex(pixels).to_string(),
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Blank { v } => {
                if *v != BACKGROUND_IDENTITY_VERSION {
                    return Err(format!("unsupported background identity version {v}"));
                }
                Ok(())
            }
            Self::Blob { v, sha256 } => {
                if *v != BACKGROUND_IDENTITY_VERSION {
                    return Err(format!("unsupported background identity version {v}"));
                }
                Sha256Hex::parse(sha256)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        }
    }

    fn stats_key(&self) -> String {
        match self {
            Self::Blank { .. } => "blank".into(),
            Self::Blob { sha256, .. } => sha256.clone(),
        }
    }
}

/// I/O attribution for snapshot / inbound background identity (thread-local, host + Kindle logs).
#[derive(Clone, Debug, Default)]
pub struct BackgroundIoStats {
    pub notebooks: usize,
    pub pages: usize,
    pub distinct_identities: BTreeSet<String>,
    pub files_read: u64,
    pub bytes_read: u64,
    pub bytes_hashed: u64,
    pub sha256_ops: u64,
    pub read_us: u64,
    pub hash_us: u64,
    pub identity_hits: u64,
    pub migrated: u64,
}

impl BackgroundIoStats {
    pub fn distinct_backgrounds(&self) -> usize {
        self.distinct_identities.len()
    }

    pub fn log_suffix(&self, total_us: u64) -> String {
        let remainder_us = total_us.saturating_sub(self.read_us.saturating_add(self.hash_us));
        format!(
            "notebooks={} pages={} distinct_backgrounds={} bg_files_read={} bg_bytes_read={} bg_bytes_hashed={} sha256_ops={} identity_hits={} migrated={} bg_read_ms={} bg_hash_ms={} remainder_ms={}",
            self.notebooks,
            self.pages,
            self.distinct_backgrounds(),
            self.files_read,
            self.bytes_read,
            self.bytes_hashed,
            self.sha256_ops,
            self.identity_hits,
            self.migrated,
            self.read_us / 1000,
            self.hash_us / 1000,
            remainder_us / 1000
        )
    }
}

thread_local! {
    static BG_IO: RefCell<BackgroundIoStats> = RefCell::new(BackgroundIoStats::default());
}

pub fn reset_background_io_stats() {
    BG_IO.with(|s| *s.borrow_mut() = BackgroundIoStats::default());
}

pub fn take_background_io_stats() -> BackgroundIoStats {
    BG_IO.with(|s| s.replace(BackgroundIoStats::default()))
}

pub fn record_snapshot_counts(notebooks: usize, pages: usize) {
    bg_io_mut(|s| {
        s.notebooks = notebooks;
        s.pages = pages;
    });
}

fn bg_io_mut(f: impl FnOnce(&mut BackgroundIoStats)) {
    BG_IO.with(|s| f(&mut s.borrow_mut()));
}

/// Parse the header `render` value by explicit `profile_version` dispatch.
fn parse_header_render(render: &serde_json::Value) -> Result<RenderProfile, String> {
    let version_val = render
        .get("profile_version")
        .ok_or_else(|| "REFUSE PAGE OPEN: render profile missing profile_version".to_string())?;
    let version = version_val
        .as_u64()
        .ok_or_else(|| "REFUSE PAGE OPEN: render profile_version must be an integer".to_string())?;
    let version = u32::try_from(version)
        .map_err(|_| "REFUSE PAGE OPEN: render profile_version out of range".to_string())?;
    match version {
        RENDER_PROFILE_VERSION_V1 => {
            let v1: PersistedRenderProfileV1 = serde_json::from_value(render.clone())
                .map_err(|e| format!("REFUSE PAGE OPEN: bad render profile v1: {e}"))?;
            validate_persisted_render_profile_v1(&v1)
        }
        RENDER_PROFILE_VERSION => {
            let v2: PersistedRenderProfileV2 = serde_json::from_value(render.clone())
                .map_err(|e| format!("REFUSE PAGE OPEN: bad render profile v2: {e}"))?;
            validate_persisted_render_profile_v2(&v2)
        }
        other => Err(format!(
            "REFUSE PAGE OPEN: unsupported render profile_version {other}"
        )),
    }
}

/// Test-only wrapper around [`parse_header_render`].
#[cfg(test)]
pub fn parse_header_render_for_test(render: &serde_json::Value) -> Result<RenderProfile, String> {
    parse_header_render(render)
}

/// Serialize a sealed profile into a header `render` value matching its version.
fn render_to_value(profile: &RenderProfile) -> Result<serde_json::Value, String> {
    match profile.profile_version {
        RENDER_PROFILE_VERSION_V1 => {
            serde_json::to_value(profile.to_persisted_v1()?).map_err(|e| e.to_string())
        }
        RENDER_PROFILE_VERSION => {
            serde_json::to_value(profile.to_persisted_v2()?).map_err(|e| e.to_string())
        }
        other => Err(format!(
            "REFUSE: cannot persist render profile_version {other}"
        )),
    }
}

/// Durable pressure rule for stroke record v2:
/// `None` = legitimate missing sample; finite in `0.0..=1.0` = allowed;
/// out-of-range / non-finite = refuse (do not clamp).
pub fn validate_persisted_pressure_v2(pressure: Option<f32>) -> Result<(), String> {
    match pressure {
        None => Ok(()),
        Some(p) if p.is_finite() && (0.0..=1.0).contains(&p) => Ok(()),
        Some(p) => Err(format!(
            "REFUSE: invalid persisted pressure {p:?} (need None or finite 0.0..=1.0)"
        )),
    }
}

/// Canonical page bounds for persisted fragment coordinates (inclusive).
/// Reject (do not clamp): NaN, ±Inf, negatives, and values beyond the page.
pub fn validate_persisted_canonical_xy(x: f32, y: f32) -> Result<(), String> {
    if !x.is_finite() || !y.is_finite() {
        return Err(format!(
            "REFUSE: non-finite fragment coordinate x={x:?} y={y:?}"
        ));
    }
    if !(0.0..=CANONICAL_WIDTH).contains(&x) || !(0.0..=CANONICAL_HEIGHT).contains(&y) {
        return Err(format!(
            "REFUSE: fragment coordinate out of canonical bounds \
             ({CANONICAL_WIDTH}x{CANONICAL_HEIGHT}): x={x} y={y}"
        ));
    }
    Ok(())
}

/// Structural validation of a `replace_strokes` payload (no visible-model checks).
///
/// Refuses malformed records before any durable append. Visibility / historical-ID
/// collision checks belong in [`prepare_replace_strokes`].
pub fn validate_replace_strokes_structural(
    replacements: &[PersistedReplacementV2],
) -> Result<(), String> {
    if replacements.is_empty() {
        return Err("REFUSE: empty replace_strokes".into());
    }
    let mut seen_targets = HashSet::new();
    let mut seen_frag_ids = HashSet::new();
    for rep in replacements {
        if rep.target_stroke_id.is_empty() {
            return Err("REFUSE: empty replace target_stroke_id".into());
        }
        if !seen_targets.insert(rep.target_stroke_id.clone()) {
            return Err(format!(
                "REFUSE: duplicate replace target {}",
                rep.target_stroke_id
            ));
        }
        for frag in &rep.fragments {
            if frag.id.is_empty() {
                return Err("REFUSE: empty fragment id".into());
            }
            if !seen_frag_ids.insert(frag.id.clone()) {
                return Err(format!("REFUSE: duplicate fragment id {}", frag.id));
            }
            // Reject unrecognized styles / empty geometry / bad coords / bad pressure.
            fragment_to_stroke(frag)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedPointV1 {
    pub x: f32,
    pub y: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedStrokeV1 {
    pub v: u32,
    #[serde(rename = "type")]
    pub record_type: String,
    pub id: String,
    pub finished: bool,
    pub points: Vec<PersistedPointV1>,
}

impl PersistedStrokeV1 {
    pub fn from_stroke(stroke: &Stroke) -> Result<Self, String> {
        if !stroke.finished {
            return Err("refuse persist unfinished stroke".into());
        }
        if stroke.points.is_empty() {
            return Err("refuse persist empty stroke".into());
        }
        match stroke.brush.as_deref() {
            None | Some(STYLE_LEGACY_FIXED_V1) => {}
            Some(STYLE_PRESSURE_WIDTH_V1) => {
                return Err("REFUSE PERSIST: pressure-width-v1 requires stroke record v2".into());
            }
            Some(other) => {
                return Err(format!(
                    "REFUSE PERSIST: unknown stroke brush style {other:?}"
                ));
            }
        }
        Ok(Self {
            v: STROKE_RECORD_VERSION,
            record_type: "stroke".into(),
            id: stroke.id.as_str().to_string(),
            finished: true,
            points: stroke
                .points
                .iter()
                .map(|p| PersistedPointV1 {
                    x: p.x,
                    y: p.y,
                    pressure: p.pressure,
                })
                .collect(),
        })
    }

    pub fn to_stroke(&self) -> Result<Stroke, String> {
        if self.v != STROKE_RECORD_VERSION {
            return Err(format!("unsupported stroke record version {}", self.v));
        }
        if self.record_type != "stroke" {
            return Err(format!("unknown record type {}", self.record_type));
        }
        if !self.finished {
            return Err(format!("unfinished persisted stroke id={}", self.id));
        }
        if self.points.is_empty() {
            return Err(format!("empty persisted stroke id={}", self.id));
        }
        Ok(Stroke {
            id: StrokeId::new(self.id.clone()),
            points: self
                .points
                .iter()
                .map(|p| {
                    let mut pt = Point::new(p.x, p.y);
                    if let Some(pr) = p.pressure {
                        pt = pt.with_pressure(pr);
                    }
                    pt
                })
                .collect(),
            brush: None,
            finished: true,
        })
    }
}

/// F6 stroke record: carries an explicit `style` tag. Written only for pressure-width strokes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedStrokeV2 {
    pub v: u32,
    #[serde(rename = "type")]
    pub record_type: String,
    pub id: String,
    pub finished: bool,
    pub style: String,
    pub points: Vec<PersistedPointV1>,
}

impl PersistedStrokeV2 {
    pub fn from_stroke(stroke: &Stroke) -> Result<Self, String> {
        if !stroke.finished {
            return Err("refuse persist unfinished stroke".into());
        }
        if stroke.points.is_empty() {
            return Err("refuse persist empty stroke".into());
        }
        let style = match stroke.brush.as_deref() {
            Some(STYLE_PRESSURE_WIDTH_V1) => STYLE_PRESSURE_WIDTH_V1.to_string(),
            other => {
                return Err(format!(
                    "PersistedStrokeV2 requires style {STYLE_PRESSURE_WIDTH_V1} (got {other:?})"
                ))
            }
        };
        let mut points = Vec::with_capacity(stroke.points.len());
        for p in &stroke.points {
            validate_persisted_pressure_v2(p.pressure)?;
            points.push(PersistedPointV1 {
                x: p.x,
                y: p.y,
                pressure: p.pressure,
            });
        }
        Ok(Self {
            v: STROKE_RECORD_VERSION_V2,
            record_type: "stroke".into(),
            id: stroke.id.as_str().to_string(),
            finished: true,
            style,
            points,
        })
    }

    pub fn to_stroke(&self) -> Result<Stroke, String> {
        if self.v != STROKE_RECORD_VERSION_V2 {
            return Err(format!("unsupported stroke record version {}", self.v));
        }
        if self.record_type != "stroke" {
            return Err(format!("unknown record type {}", self.record_type));
        }
        if self.style != STYLE_PRESSURE_WIDTH_V1 {
            return Err(format!("unsupported stroke style {}", self.style));
        }
        if !self.finished {
            return Err(format!("unfinished persisted stroke id={}", self.id));
        }
        if self.points.is_empty() {
            return Err(format!("empty persisted stroke id={}", self.id));
        }
        let mut points = Vec::with_capacity(self.points.len());
        for p in &self.points {
            validate_persisted_pressure_v2(p.pressure)?;
            let mut pt = Point::new(p.x, p.y);
            if let Some(pr) = p.pressure {
                pt = pt.with_pressure(pr);
            }
            points.push(pt);
        }
        Ok(Stroke {
            id: StrokeId::new(self.id.clone()),
            points,
            brush: Some(self.style.clone()),
            finished: true,
        })
    }
}

/// Serialize a finished stroke to its JSONL line.
///
/// ```text
/// brush = None | "legacy-fixed-v1"  → stroke record v1
/// brush = "pressure-width-v1"      → stroke record v2
/// anything else                    → ERROR (never silently drop style)
/// ```
pub fn stroke_record_line(stroke: &Stroke) -> Result<String, String> {
    let mut line = match stroke.brush.as_deref() {
        None | Some(STYLE_LEGACY_FIXED_V1) => {
            serde_json::to_string(&PersistedStrokeV1::from_stroke(stroke)?)
                .map_err(|e| e.to_string())?
        }
        Some(STYLE_PRESSURE_WIDTH_V1) => {
            serde_json::to_string(&PersistedStrokeV2::from_stroke(stroke)?)
                .map_err(|e| e.to_string())?
        }
        Some(other) => {
            return Err(format!(
                "REFUSE PERSIST: unknown stroke brush style {other:?}"
            ));
        }
    };
    line.push('\n');
    Ok(line)
}

/// Parse one stroke JSONL record (v1 legacy or v2 pressure) by peeking `v`.
fn parse_stroke_record(line: &[u8]) -> Result<Stroke, String> {
    #[derive(Deserialize)]
    struct VersionPeek {
        v: u32,
    }
    let peek: VersionPeek = serde_json::from_slice(line)
        .map_err(|e| format!("REFUSE PAGE OPEN: malformed journal record: {e}"))?;
    match peek.v {
        STROKE_RECORD_VERSION => {
            let rec: PersistedStrokeV1 = serde_json::from_slice(line)
                .map_err(|e| format!("REFUSE PAGE OPEN: malformed journal record: {e}"))?;
            rec.to_stroke()
        }
        STROKE_RECORD_VERSION_V2 => {
            let rec: PersistedStrokeV2 = serde_json::from_slice(line)
                .map_err(|e| format!("REFUSE PAGE OPEN: malformed journal record: {e}"))?;
            rec.to_stroke()
        }
        other => Err(format!(
            "REFUSE PAGE OPEN: unsupported stroke record version {other}"
        )),
    }
}

/// Append-only delete tombstone (`edits.jsonl`, edit record v1).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedDeleteStrokeV1 {
    pub v: u32,
    #[serde(rename = "type")]
    pub record_type: String,
    pub stroke_id: String,
}

impl PersistedDeleteStrokeV1 {
    #[allow(dead_code)] // used by append_delete_stroke + framing tests
    pub fn new(stroke_id: &StrokeId) -> Self {
        Self {
            v: EDIT_RECORD_VERSION_V1,
            record_type: "delete_stroke".into(),
            stroke_id: stroke_id.as_str().to_string(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.v != EDIT_RECORD_VERSION_V1 {
            return Err(format!(
                "REFUSE PAGE OPEN: unsupported edit record version {}",
                self.v
            ));
        }
        if self.record_type != "delete_stroke" {
            return Err(format!(
                "REFUSE PAGE OPEN: unknown edit record type {}",
                self.record_type
            ));
        }
        if self.stroke_id.is_empty() {
            return Err("REFUSE PAGE OPEN: empty delete stroke_id".into());
        }
        Ok(())
    }
}

/// One fragment in a v2 `replace_strokes` edit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedFragmentV2 {
    pub id: String,
    /// Absent / omitted → legacy-fixed (same as stroke journal v1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    pub points: Vec<PersistedPointV1>,
}

/// One target replacement inside a v2 `replace_strokes` edit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedReplacementV2 {
    pub target_stroke_id: String,
    pub fragments: Vec<PersistedFragmentV2>,
}

/// Atomic multi-target replacement (`edits.jsonl`, edit record v2).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedReplaceStrokesV2 {
    pub v: u32,
    #[serde(rename = "type")]
    pub record_type: String,
    pub replacements: Vec<PersistedReplacementV2>,
}

impl PersistedReplaceStrokesV2 {
    pub fn validate_shape(&self) -> Result<(), String> {
        if self.v != EDIT_RECORD_VERSION_V2 {
            return Err(format!(
                "REFUSE PAGE OPEN: unsupported edit record version {}",
                self.v
            ));
        }
        if self.record_type != "replace_strokes" {
            return Err(format!(
                "REFUSE PAGE OPEN: unknown edit record type {}",
                self.record_type
            ));
        }
        if self.replacements.is_empty() {
            return Err("REFUSE PAGE OPEN: replace_strokes with empty replacements".into());
        }
        Ok(())
    }
}

/// Parsed edit-journal operation (v1 delete or v2 replace).
#[derive(Clone, Debug, PartialEq)]
pub enum EditOp {
    DeleteStroke {
        stroke_id: String,
    },
    ReplaceStrokes {
        replacements: Vec<PersistedReplacementV2>,
    },
}

fn parse_edit_record_line(line: &[u8]) -> Result<EditOp, String> {
    #[derive(Deserialize)]
    struct Peek {
        v: u32,
        #[serde(rename = "type")]
        record_type: String,
    }
    let peek: Peek = serde_json::from_slice(line)
        .map_err(|e| format!("REFUSE PAGE OPEN: malformed edit journal record: {e}"))?;
    match (peek.v, peek.record_type.as_str()) {
        (EDIT_RECORD_VERSION_V1, "delete_stroke") => {
            let rec: PersistedDeleteStrokeV1 = serde_json::from_slice(line)
                .map_err(|e| format!("REFUSE PAGE OPEN: malformed edit journal record: {e}"))?;
            rec.validate()?;
            Ok(EditOp::DeleteStroke {
                stroke_id: rec.stroke_id,
            })
        }
        (EDIT_RECORD_VERSION_V2, "replace_strokes") => {
            let rec: PersistedReplaceStrokesV2 = serde_json::from_slice(line)
                .map_err(|e| format!("REFUSE PAGE OPEN: malformed edit journal record: {e}"))?;
            rec.validate_shape()?;
            Ok(EditOp::ReplaceStrokes {
                replacements: rec.replacements,
            })
        }
        (v, ty) => Err(format!(
            "REFUSE PAGE OPEN: unsupported edit record v={v} type={ty}"
        )),
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PersistTiming {
    pub append_us: u64,
    pub fsync_us: u64,
}

impl PersistTiming {
    pub fn commit_us(self) -> u64 {
        self.append_us.saturating_add(self.fsync_us)
    }
}

#[derive(Clone, Debug)]
pub struct LoadedPage {
    pub header: PersistedPageHeader,
    pub background: Vec<u8>,
    pub strokes: Vec<Stroke>,
    /// Authored-stroke lineage: visible stroke id → root id.
    pub root_of: std::collections::HashMap<String, String>,
    /// True when the journal ended with an incomplete final line that was ignored.
    pub truncated_tail: bool,
    pub page_open: PageOpenKind,
    /// Sealed render profile validated against this binary.
    pub render_profile: RenderProfile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageOpenKind {
    New,
    Existing,
}

/// Small durable page interface (create / load / append stroke / append delete / replace).
pub trait PageStore {
    fn append_committed_stroke(&mut self, stroke: &Stroke) -> Result<PersistTiming, String>;
    #[allow(dead_code)] // F3/F5 whole-stroke delete; F7 uses [`Self::append_replace_strokes`]
    fn append_delete_stroke(&mut self, stroke_id: &StrokeId) -> Result<PersistTiming, String>;
    fn append_replace_strokes(
        &mut self,
        replacements: &[PersistedReplacementV2],
    ) -> Result<PersistTiming, String>;
    fn is_writable(&self) -> bool;
    fn stroke_count(&self) -> usize;
    /// True if this page ever received durable authored stroke/root ids (not merely visible count).
    ///
    /// A page emptied by erase/undo still reports true; a never-used pristine page reports false.
    fn has_authored_history(&self) -> bool;
}

/// Kindle session store: unbound at Home launch, `FsPageStore` after a notebook is opened.
/// Bound is the live store; boxing it would heap-allocate on every page switch.
#[allow(clippy::large_enum_variant)]
pub enum RuntimeStore {
    Unbound,
    Bound(FsPageStore),
}

impl RuntimeStore {
    pub fn is_bound(&self) -> bool {
        matches!(self, Self::Bound(_))
    }
}

impl PageStore for RuntimeStore {
    fn append_committed_stroke(&mut self, stroke: &Stroke) -> Result<PersistTiming, String> {
        match self {
            Self::Unbound => Err("REFUSE: no notebook bound".into()),
            Self::Bound(s) => s.append_committed_stroke(stroke),
        }
    }
    fn append_delete_stroke(&mut self, stroke_id: &StrokeId) -> Result<PersistTiming, String> {
        match self {
            Self::Unbound => Err("REFUSE: no notebook bound".into()),
            Self::Bound(s) => s.append_delete_stroke(stroke_id),
        }
    }
    fn append_replace_strokes(
        &mut self,
        replacements: &[PersistedReplacementV2],
    ) -> Result<PersistTiming, String> {
        match self {
            Self::Unbound => Err("REFUSE: no notebook bound".into()),
            Self::Bound(s) => s.append_replace_strokes(replacements),
        }
    }
    fn is_writable(&self) -> bool {
        match self {
            Self::Unbound => false,
            Self::Bound(s) => s.is_writable(),
        }
    }
    fn stroke_count(&self) -> usize {
        match self {
            Self::Unbound => 0,
            Self::Bound(s) => s.stroke_count(),
        }
    }
    fn has_authored_history(&self) -> bool {
        match self {
            Self::Unbound => false,
            Self::Bound(s) => s.has_authored_history(),
        }
    }
}

/// In-memory store for host tests / fault injection.
#[derive(Debug, Default)]
#[allow(dead_code)] // exercised by host unit tests only
pub struct MemoryPageStore {
    pub strokes: Vec<Stroke>,
    pub known_ids: HashSet<String>,
    pub root_of: std::collections::HashMap<String, String>,
    pub deleted_ids: HashSet<String>,
    pub fail_append: bool,
    pub fail_flush: bool,
    pub fail_delete: bool,
    pub fail_delete_fsync: bool,
    /// Counts successful `append_replace_strokes` calls (host tests).
    pub replace_append_count: u64,
    pub writable: bool,
}

impl MemoryPageStore {
    pub fn new() -> Self {
        Self {
            writable: true,
            ..Default::default()
        }
    }
}

impl PageStore for MemoryPageStore {
    fn append_committed_stroke(&mut self, stroke: &Stroke) -> Result<PersistTiming, String> {
        if !self.writable {
            return Err("store not writable".into());
        }
        if self.fail_append {
            return Err("fake append fail".into());
        }
        if self.fail_flush {
            return Err("fake fsync fail".into());
        }
        let _ = stroke_record_line(stroke)?;
        let id = stroke.id.as_str().to_string();
        if !self.known_ids.insert(id.clone()) {
            return Err(format!("duplicate stroke id {id}"));
        }
        self.root_of.insert(id.clone(), id);
        self.strokes.push(stroke.clone());
        Ok(PersistTiming {
            append_us: 10,
            fsync_us: 20,
        })
    }

    fn append_delete_stroke(&mut self, stroke_id: &StrokeId) -> Result<PersistTiming, String> {
        if !self.writable {
            return Err("store not writable".into());
        }
        if self.fail_delete {
            return Err("fake delete append fail".into());
        }
        if self.fail_delete_fsync {
            return Err("fake delete fsync fail".into());
        }
        let id = stroke_id.as_str().to_string();
        if !self.known_ids.contains(&id) {
            return Err(format!("REFUSE: delete unknown stroke id {id}"));
        }
        if self.deleted_ids.contains(&id) {
            return Err(format!("REFUSE: duplicate delete stroke id {id}"));
        }
        // Durable before model mutation (caller removes from session after Ok).
        self.deleted_ids.insert(id.clone());
        self.strokes.retain(|s| s.id.as_str() != id);
        Ok(PersistTiming {
            append_us: 11,
            fsync_us: 21,
        })
    }

    fn append_replace_strokes(
        &mut self,
        replacements: &[PersistedReplacementV2],
    ) -> Result<PersistTiming, String> {
        if !self.writable {
            return Err("store not writable".into());
        }
        if self.fail_delete {
            return Err("fake replace append fail".into());
        }
        // Structural only — semantic/visibility preflight belongs to the session.
        validate_replace_strokes_structural(replacements)?;
        let mut model = VisibleModel {
            strokes: self.strokes.clone(),
            root_of: self.root_of.clone(),
            known_ids: self.known_ids.clone(),
        };
        // Keep store strokes aligned when possible (session already prepared).
        match apply_replace_strokes(&mut model, replacements.to_vec()) {
            Ok(()) => {
                self.strokes = model.strokes;
                self.root_of = model.root_of;
                self.known_ids = model.known_ids;
            }
            Err(_) => {
                for rep in replacements {
                    for frag in &rep.fragments {
                        self.known_ids.insert(frag.id.clone());
                    }
                }
            }
        }
        self.replace_append_count += 1;
        Ok(PersistTiming {
            append_us: 12,
            fsync_us: 22,
        })
    }

    fn is_writable(&self) -> bool {
        self.writable
    }

    fn stroke_count(&self) -> usize {
        self.strokes.len()
    }

    fn has_authored_history(&self) -> bool {
        !self.known_ids.is_empty()
    }
}

/// On-disk page under `{data_dir}/{page_id}/`.
pub struct FsPageStore {
    dir: PathBuf,
    header: PersistedPageHeader,
    journal: File,
    edits: File,
    stroke_count: usize,
    known_ids: HashSet<String>,
    #[allow(dead_code)] // updated by append_delete_stroke (F3 path)
    deleted_ids: HashSet<String>,
    writable: bool,
}

fn meta_path(dir: &Path) -> PathBuf {
    dir.join("page.meta")
}
fn bg_path(dir: &Path) -> PathBuf {
    dir.join("background.raw")
}
fn journal_path(dir: &Path) -> PathBuf {
    dir.join("strokes.jsonl")
}
fn edits_path(dir: &Path) -> PathBuf {
    dir.join("edits.jsonl")
}

/// Durable page text objects. Absent file = no texts.
pub fn texts_path(dir: &Path) -> PathBuf {
    dir.join("texts.jsonl")
}

/// Durable page links. Absent file = no links.
pub fn links_path(dir: &Path) -> PathBuf {
    dir.join("links.jsonl")
}

/// Load `texts.jsonl` (missing → empty). Each line is one `SyncText`.
pub fn load_page_texts(dir: &Path) -> Result<Vec<sync_protocol::SyncText>, String> {
    let path = texts_path(dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let t: sync_protocol::SyncText =
            serde_json::from_str(line).map_err(|e| format!("texts.jsonl line {}: {e}", i + 1))?;
        t.validate()
            .map_err(|e| format!("texts.jsonl line {}: {e}", i + 1))?;
        out.push(t);
    }
    Ok(out)
}

/// Replace `texts.jsonl` atomically. Empty list removes the file.
pub fn write_page_texts(dir: &Path, texts: &[sync_protocol::SyncText]) -> Result<(), String> {
    let _admit = crate::sync_verified::note_mutation_from_page_dir(dir)?;
    let path = texts_path(dir);
    if texts.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    let mut body = String::new();
    for t in texts {
        t.validate().map_err(|e| e.to_string())?;
        let line =
            String::from_utf8(sync_protocol::to_canonical_json(t).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        body.push_str(&line);
        body.push('\n');
    }
    write_file_atomic(dir, "texts.jsonl", "texts.jsonl.tmp", body.as_bytes())
}

/// Load `links.jsonl` (missing → empty). Each line is one `SyncPageLink`.
pub fn load_page_links(dir: &Path) -> Result<Vec<sync_protocol::SyncPageLink>, String> {
    let path = links_path(dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let t: sync_protocol::SyncPageLink =
            serde_json::from_str(line).map_err(|e| format!("links.jsonl line {}: {e}", i + 1))?;
        t.validate()
            .map_err(|e| format!("links.jsonl line {}: {e}", i + 1))?;
        out.push(t);
    }
    Ok(out)
}

/// Replace `links.jsonl` atomically. Empty list removes the file.
pub fn write_page_links(dir: &Path, links: &[sync_protocol::SyncPageLink]) -> Result<(), String> {
    let _admit = crate::sync_verified::note_mutation_from_page_dir(dir)?;
    let path = links_path(dir);
    if links.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    let mut body = String::new();
    for t in links {
        t.validate().map_err(|e| e.to_string())?;
        let line =
            String::from_utf8(sync_protocol::to_canonical_json(t).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        body.push_str(&line);
        body.push('\n');
    }
    write_file_atomic(dir, "links.jsonl", "links.jsonl.tmp", body.as_bytes())
}

pub fn resolve_data_dir(args: &[String]) -> PathBuf {
    args.windows(2)
        .find(|w| w[0] == "--data-dir")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR))
}

pub fn page_dir(data_dir: &Path, page_id: &str) -> PathBuf {
    data_dir.join(page_id)
}

fn validate_header(h: &PersistedPageHeader) -> Result<RenderProfile, String> {
    if h.v != FORMAT_VERSION {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported format_version {} (need {FORMAT_VERSION})",
            h.v
        ));
    }
    if h.edit_journal_version != EDIT_JOURNAL_VERSION {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported edit_journal_version {} (need {EDIT_JOURNAL_VERSION})",
            h.edit_journal_version
        ));
    }
    if (h.canonical_width - CANONICAL_WIDTH).abs() > f32::EPSILON
        || (h.canonical_height - CANONICAL_HEIGHT).abs() > f32::EPSILON
    {
        return Err(format!(
            "REFUSE PAGE OPEN: wrong page geometry {}x{} (need {CANONICAL_WIDTH}x{CANONICAL_HEIGHT})",
            h.canonical_width, h.canonical_height
        ));
    }
    if h.bg_layout_version != BG_LAYOUT_VERSION {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported bg_layout_version {}",
            h.bg_layout_version
        ));
    }
    parse_header_render(&h.render)
}

/// Ensure live framebuffer matches persisted panel assumptions before restore.
pub fn validate_live_framebuffer(h: &PersistedPageHeader, geo: &FbGeometry) -> Result<(), String> {
    validate_framebuffer_vs_page(
        h.panel_width,
        h.panel_height,
        h.line_length,
        h.bg_layout_version,
        BG_LAYOUT_VERSION,
        geo,
    )
}

/// Parse journal bytes. Only newline-terminated records are committed.
///
/// ```text
/// nonempty bytes after the final newline → incomplete tail → NEVER applied
/// (even if those bytes form valid JSON)
/// malformed complete interior record → fail closed
/// ```
pub fn parse_stroke_journal(raw: &[u8]) -> Result<(Vec<Stroke>, bool), String> {
    let mut strokes = Vec::new();
    let mut ids = HashSet::new();
    let mut start = 0usize;
    let mut saw_newline = false;
    for i in 0..raw.len() {
        if raw[i] != b'\n' {
            continue;
        }
        saw_newline = true;
        let line = &raw[start..i];
        start = i + 1;
        if line.is_empty() || (line.len() == 1 && line[0] == b'\r') {
            continue;
        }
        let line = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        let stroke = parse_stroke_record(line)?;
        if !ids.insert(stroke.id.as_str().to_string()) {
            return Err(format!(
                "REFUSE PAGE OPEN: duplicate stroke id {}",
                stroke.id
            ));
        }
        strokes.push(stroke);
    }
    // Incomplete tail: any bytes after the last newline (or entire buffer if no newline).
    let truncated_tail = if start < raw.len() {
        !raw[start..].is_empty()
    } else {
        false
    };
    if truncated_tail && !saw_newline && raw.iter().any(|&b| !b.is_ascii_whitespace()) {
        // Entire journal is a non-newline-terminated record → nothing committed.
        eprintln!("warning: truncated journal tail ignored (no terminating newline)");
    } else if truncated_tail {
        eprintln!("warning: truncated journal tail ignored (incomplete final record)");
    }
    Ok((strokes, truncated_tail))
}

/// Parse edits.jsonl. Same newline-commit framing as [`parse_stroke_journal`].
///
/// Returns deleted stroke ids (v1 `delete_stroke` only). F7 replaces this with full
/// edit replay via [`replay_edit_journal`].
#[allow(dead_code)] // retained for v1-only framing tests / tooling
pub fn parse_edit_journal(
    raw: &[u8],
    known_stroke_ids: &HashSet<String>,
) -> Result<(Vec<String>, bool), String> {
    let (ops, truncated_tail) = parse_edit_records(raw)?;
    let mut deleted = Vec::new();
    let mut deleted_set = HashSet::new();
    for op in ops {
        match op {
            EditOp::DeleteStroke { stroke_id } => {
                if !known_stroke_ids.contains(&stroke_id) {
                    return Err(format!(
                        "REFUSE PAGE OPEN: delete targets unknown stroke id {stroke_id}"
                    ));
                }
                if !deleted_set.insert(stroke_id.clone()) {
                    return Err(format!(
                        "REFUSE PAGE OPEN: duplicate delete stroke id {stroke_id}"
                    ));
                }
                deleted.push(stroke_id);
            }
            EditOp::ReplaceStrokes { .. } => {
                return Err(
                    "REFUSE PAGE OPEN: replace_strokes requires F7 edit replay path".into(),
                );
            }
        }
    }
    Ok((deleted, truncated_tail))
}

/// Split edit journal into newline-committed records (no semantic apply).
fn parse_edit_records(raw: &[u8]) -> Result<(Vec<EditOp>, bool), String> {
    let mut ops = Vec::new();
    let mut start = 0usize;
    for i in 0..raw.len() {
        if raw[i] != b'\n' {
            continue;
        }
        let line = &raw[start..i];
        start = i + 1;
        if line.is_empty() || (line.len() == 1 && line[0] == b'\r') {
            continue;
        }
        let line = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        ops.push(parse_edit_record_line(line)?);
    }
    let truncated_tail = start < raw.len() && !raw[start..].is_empty();
    if truncated_tail {
        eprintln!("warning: truncated edit journal tail ignored (incomplete final record)");
    }
    Ok((ops, truncated_tail))
}

/// Visible page state after replaying the edit journal over base strokes.
#[derive(Clone, Debug, Default)]
pub struct VisibleModel {
    pub strokes: Vec<Stroke>,
    /// stroke_id → authored root lineage id
    pub root_of: std::collections::HashMap<String, String>,
    /// All ids ever present in base journal or introduced as fragments.
    pub known_ids: HashSet<String>,
}

impl VisibleModel {
    pub fn from_base_strokes(base: Vec<Stroke>) -> Self {
        let mut root_of = std::collections::HashMap::new();
        let mut known_ids = HashSet::new();
        for s in &base {
            let id = s.id.as_str().to_string();
            known_ids.insert(id.clone());
            root_of.insert(id.clone(), id);
        }
        Self {
            strokes: base,
            root_of,
            known_ids,
        }
    }
}

/// Replay newline-committed edit records over base strokes (atomic per record).
pub fn replay_edit_journal(
    base: Vec<Stroke>,
    edit_raw: &[u8],
) -> Result<(VisibleModel, bool), String> {
    let (ops, truncated) = parse_edit_records(edit_raw)?;
    let mut model = VisibleModel::from_base_strokes(base);
    for op in ops {
        apply_edit_op(&mut model, op)?;
    }
    Ok((model, truncated))
}

fn apply_edit_op(model: &mut VisibleModel, op: EditOp) -> Result<(), String> {
    match op {
        EditOp::DeleteStroke { stroke_id } => {
            if !model.strokes.iter().any(|s| s.id.as_str() == stroke_id) {
                // Historical: v1 deletes targeted base strokes that must have existed in the
                // journal; after prior deletes they may already be absent from visible list but
                // still in known_ids. Pre-F7 required known_ids.contains. Keep that:
                if !model.known_ids.contains(&stroke_id) {
                    return Err(format!(
                        "REFUSE PAGE OPEN: delete targets unknown stroke id {stroke_id}"
                    ));
                }
                // If already not visible, treat as duplicate tombstone.
                return Err(format!(
                    "REFUSE PAGE OPEN: duplicate delete stroke id {stroke_id}"
                ));
            }
            // Also refuse second delete of same id via tracking removed set — use absence from
            // visible + a deleted set. Simpler: remove from visible; if not found above we erred.
            // Track deletes in known sense: remove from visible only once.
            let before = model.strokes.len();
            model
                .strokes
                .retain(|s| s.id.as_str() != stroke_id.as_str());
            if model.strokes.len() == before {
                return Err(format!(
                    "REFUSE PAGE OPEN: duplicate delete stroke id {stroke_id}"
                ));
            }
            Ok(())
        }
        EditOp::ReplaceStrokes { replacements } => apply_replace_strokes(model, replacements),
    }
}

pub fn apply_replace_strokes(
    model: &mut VisibleModel,
    replacements: Vec<PersistedReplacementV2>,
) -> Result<(), String> {
    let prepared = prepare_replace_strokes(model, &replacements)?;
    model.strokes = prepared.strokes;
    model.root_of = prepared.root_of;
    model.known_ids = prepared.known_ids;
    Ok(())
}

/// Prepared visible-model state after a validated `replace_strokes` (not yet installed).
#[derive(Clone, Debug)]
pub struct PreparedReplace {
    pub strokes: Vec<Stroke>,
    pub root_of: std::collections::HashMap<String, String>,
    pub known_ids: HashSet<String>,
}

/// Validate + build the post-replace visible model without mutating `model`.
///
/// Live session preflight and restart replay share this path so validators cannot diverge.
pub fn prepare_replace_strokes(
    model: &VisibleModel,
    replacements: &[PersistedReplacementV2],
) -> Result<PreparedReplace, String> {
    validate_replace_strokes_structural(replacements)?;

    let mut seen_targets = HashSet::new();
    let mut new_frag_ids = HashSet::new();
    let mut planned: Vec<(usize, String, String, Vec<Stroke>)> = Vec::new();
    // (index, target_id, root_id, fragments)

    for rep in replacements {
        // Structural checks already covered emptiness / dups; re-check target visibility here.
        if !seen_targets.insert(rep.target_stroke_id.clone()) {
            return Err(format!(
                "REFUSE PAGE OPEN: duplicate replace target {}",
                rep.target_stroke_id
            ));
        }
        let idx = model
            .strokes
            .iter()
            .position(|s| s.id.as_str() == rep.target_stroke_id)
            .ok_or_else(|| {
                format!(
                    "REFUSE PAGE OPEN: replace targets unknown/non-visible stroke id {}",
                    rep.target_stroke_id
                )
            })?;
        let root_id = model
            .root_of
            .get(&rep.target_stroke_id)
            .cloned()
            .unwrap_or_else(|| rep.target_stroke_id.clone());
        let mut frags = Vec::new();
        for frag in &rep.fragments {
            // A one-for-one replacement may keep the target ID. This is the narrow geometry
            // rewrite used by durable lineage moves: identity and root stay unchanged while
            // only canonical points change. Erase fragments still require fresh IDs.
            let preserves_target = rep.fragments.len() == 1 && frag.id == rep.target_stroke_id;
            if preserves_target && frag.style != model.strokes[idx].brush {
                return Err(format!(
                    "REFUSE PAGE OPEN: same-id rewrite changes style {}",
                    frag.id
                ));
            }
            if (model.known_ids.contains(&frag.id) && !preserves_target)
                || new_frag_ids.contains(&frag.id)
            {
                return Err(format!(
                    "REFUSE PAGE OPEN: fragment id collides {}",
                    frag.id
                ));
            }
            if !new_frag_ids.insert(frag.id.clone()) {
                return Err(format!(
                    "REFUSE PAGE OPEN: duplicate fragment id {}",
                    frag.id
                ));
            }
            let stroke = fragment_to_stroke(frag)?;
            frags.push(stroke);
        }
        planned.push((idx, rep.target_stroke_id.clone(), root_id, frags));
    }

    planned.sort_by_key(|(idx, _, _, _)| *idx);
    let mut out = Vec::new();
    let mut known_ids = model.known_ids.clone();
    let mut root_of = model.root_of.clone();
    let mut cursor = 0usize;
    for (idx, _target_id, root_id, frags) in planned {
        while cursor < idx {
            out.push(model.strokes[cursor].clone());
            cursor += 1;
        }
        cursor = idx + 1;
        for frag in frags {
            let fid = frag.id.as_str().to_string();
            known_ids.insert(fid.clone());
            root_of.insert(fid, root_id.clone());
            out.push(frag);
        }
    }
    while cursor < model.strokes.len() {
        out.push(model.strokes[cursor].clone());
        cursor += 1;
    }
    Ok(PreparedReplace {
        strokes: out,
        root_of,
        known_ids,
    })
}

fn fragment_to_stroke(frag: &PersistedFragmentV2) -> Result<Stroke, String> {
    if frag.points.is_empty() {
        return Err(format!(
            "REFUSE PAGE OPEN: empty fragment geometry id={}",
            frag.id
        ));
    }
    let brush = match frag.style.as_deref() {
        None => None,
        Some(STYLE_LEGACY_FIXED_V1) => Some(STYLE_LEGACY_FIXED_V1.to_string()),
        Some(STYLE_PRESSURE_WIDTH_V1) => Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        Some(other) => {
            return Err(format!("REFUSE PAGE OPEN: invalid fragment style {other}"));
        }
    };
    let pressure_style = matches!(brush.as_deref(), Some(STYLE_PRESSURE_WIDTH_V1));
    let mut points = Vec::with_capacity(frag.points.len());
    for p in &frag.points {
        validate_persisted_canonical_xy(p.x, p.y)?;
        if pressure_style {
            validate_persisted_pressure_v2(p.pressure)?;
        }
        let mut pt = Point::new(p.x, p.y);
        if let Some(pr) = p.pressure {
            pt.pressure = Some(pr);
        }
        points.push(pt);
    }
    Ok(Stroke {
        id: StrokeId::new(frag.id.clone()),
        points,
        brush,
        finished: true,
    })
}

/// Apply tombstones in order; returns visible strokes.
/// Prefer [`replay_edit_journal`] for F7 (handles replace_strokes + lineage).
#[allow(dead_code)] // retained for pre-F7 delete-only helpers / tests
pub fn apply_delete_tombstones(strokes: Vec<Stroke>, deleted_ids: &[String]) -> Vec<Stroke> {
    if deleted_ids.is_empty() {
        return strokes;
    }
    let del: HashSet<&str> = deleted_ids.iter().map(|s| s.as_str()).collect();
    strokes
        .into_iter()
        .filter(|s| !del.contains(s.id.as_str()))
        .collect()
}

/// Atomic replace of a file in `dir`: tmp → fsync → rename (same pattern as
/// [`write_background_atomic`]). On Windows, `rename` cannot replace an existing dest, so the
/// dest is removed first — the only extra step needed for a valid replace on that host.
fn write_file_atomic(
    dir: &Path,
    dest_name: &str,
    tmp_name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let _admit = crate::sync_verified::note_mutation_from_page_dir(dir)?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let dest = dir.join(dest_name);
    let tmp = dir.join(tmp_name);
    {
        let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(bytes).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    #[cfg(windows)]
    if dest.exists() {
        fs::remove_file(&dest).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        if let Ok(dirf) = File::open(dir) {
            let _ = dirf.sync_all();
        }
    }
    Ok(())
}

/// Atomic write of background bytes: tmp → fsync → rename.
///
/// This is the only legitimate writer of `background.raw`. Callers that replace an existing
/// background must go through [`install_background`] so the persisted identity cannot skip-read
/// a stale hash. Raw use is reserved for first-time page create (no `page.meta` yet).
pub fn write_background_atomic(dir: &Path, pixels: &[u8]) -> Result<(), String> {
    write_file_atomic(dir, "background.raw", "background.raw.tmp", pixels)
}

pub fn write_meta(dir: &Path, header: &PersistedPageHeader) -> Result<(), String> {
    let _admit = crate::sync_verified::note_mutation_from_page_dir(dir)?;
    let path = meta_path(dir);
    let tmp = dir.join("page.meta.tmp");
    let json = serde_json::to_string_pretty(header).map_err(|e| e.to_string())?;
    {
        let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
        f.write_all(b"\n").map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Replace `background.raw` and publish a matching checksum + sync identity in `page.meta`.
///
/// Crash semantics:
/// 1. Existing identity is cleared first (a skip-read cannot use a hash for bytes not yet written).
/// 2. New bytes are made durable.
/// 3. Checksum + identity are published together. Identity is never visible without matching
///    checksum metadata for those bytes.
///
/// A crash between (2) and (3) leaves identity absent; the next reconstruct reads the file,
/// verifies checksum, and either refuses (mismatch) or migrates once.
pub fn install_background(
    dir: &Path,
    pixels: &[u8],
    header: &mut PersistedPageHeader,
) -> Result<(), String> {
    let _admit = crate::sync_verified::note_mutation_from_page_dir(dir)?;
    if meta_path(dir).exists() {
        header.background_identity = None;
        write_meta(dir, header)?;
    }
    write_background_atomic(dir, pixels)?;
    header.background_bytes = pixels.len() as u64;
    header.background_checksum = rect_checksum(pixels);
    header.background_identity = Some(BackgroundIdentityV1::from_pixels(pixels));
    write_meta(dir, header)
}

pub fn create_new_page(
    data_dir: &Path,
    page_id: &str,
    panel_width: u32,
    panel_height: u32,
    line_length: u32,
    background: &[u8],
    profile: &RenderProfile,
) -> Result<(FsPageStore, LoadedPage), String> {
    let dir = page_dir(data_dir, page_id);
    if meta_path(&dir).exists()
        || journal_path(&dir).exists()
        || bg_path(&dir).exists()
        || edits_path(&dir).exists()
    {
        return Err(format!(
            "REFUSE PAGE CREATE: path already has page data ({})",
            dir.display()
        ));
    }
    let _admit = crate::sync_verified::note_mutation_from_notebook_dir(data_dir)?;
    // Re-validate so create cannot persist an unsupported profile.
    let render_value = render_to_value(profile)?;
    let profile = parse_header_render(&render_value)?;
    let expected = (line_length as u64).saturating_mul(panel_height as u64);
    // Prefer packed visible width when buffer is xres*yres (extract_rect style).
    let bg_len = background.len() as u64;
    if bg_len != expected && bg_len != (panel_width as u64) * (panel_height as u64) {
        return Err(format!(
            "background byte count {bg_len} matches neither stride*{panel_height}={expected} nor {panel_width}*{panel_height}"
        ));
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    write_background_atomic(&dir, background)?;
    // Edit journal must exist before v3 meta is durable.
    {
        let ef = File::create(edits_path(&dir)).map_err(|e| e.to_string())?;
        ef.sync_all().map_err(|e| e.to_string())?;
    }
    let header = PersistedPageHeader {
        v: FORMAT_VERSION,
        page_id: page_id.to_string(),
        canonical_width: CANONICAL_WIDTH,
        canonical_height: CANONICAL_HEIGHT,
        panel_width,
        panel_height,
        line_length,
        background_bytes: bg_len,
        background_checksum: rect_checksum(background),
        bg_layout_version: BG_LAYOUT_VERSION,
        render: render_value,
        edit_journal_version: EDIT_JOURNAL_VERSION,
        background_identity: Some(BackgroundIdentityV1::from_pixels(background)),
    };
    write_meta(&dir, &header)?;
    {
        let jf = File::create(journal_path(&dir)).map_err(|e| e.to_string())?;
        jf.sync_all().map_err(|e| e.to_string())?;
    }
    let journal = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(journal_path(&dir))
        .map_err(|e| e.to_string())?;
    let edits = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(edits_path(&dir))
        .map_err(|e| e.to_string())?;
    let store = FsPageStore {
        dir,
        header: header.clone(),
        journal,
        edits,
        stroke_count: 0,
        known_ids: HashSet::new(),
        deleted_ids: HashSet::new(),
        writable: true,
    };
    let loaded = LoadedPage {
        header,
        background: background.to_vec(),
        strokes: Vec::new(),
        root_of: std::collections::HashMap::new(),
        truncated_tail: false,
        page_open: PageOpenKind::New,
        render_profile: profile,
    };
    Ok((store, loaded))
}

/// Idempotent v2→v3 migration. Does not rewrite strokes.jsonl or background.raw.
pub fn migrate_page_v2_to_v3_if_needed(data_dir: &Path, page_id: &str) -> Result<bool, String> {
    let dir = page_dir(data_dir, page_id);
    let meta_p = meta_path(&dir);
    if !meta_p.exists() {
        return Err(format!(
            "REFUSE PAGE OPEN: missing page.meta in {}",
            dir.display()
        ));
    }
    let meta_s = fs::read_to_string(&meta_p).map_err(|e| e.to_string())?;
    let mut header: PersistedPageHeader = serde_json::from_str(&meta_s)
        .map_err(|e| format!("REFUSE PAGE OPEN: bad page.meta: {e}"))?;
    if header.v >= FORMAT_VERSION_V3 {
        if !edits_path(&dir).exists() {
            return Err(format!(
                "REFUSE PAGE OPEN: format v{} page missing edits.jsonl ({})",
                header.v,
                dir.display()
            ));
        }
        return Ok(false);
    }
    if header.v != FORMAT_VERSION_V2 {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported format_version {} (need {FORMAT_VERSION_V2}+)",
            header.v
        ));
    }
    // Crash after edits create, before meta rename: still v2 → safe to recreate empty edits.
    let _admit = crate::sync_verified::note_mutation_from_page_dir(&dir)?;
    {
        let ef = File::create(edits_path(&dir)).map_err(|e| e.to_string())?;
        ef.sync_all().map_err(|e| e.to_string())?;
    }
    header.v = FORMAT_VERSION_V3;
    header.edit_journal_version = EDIT_JOURNAL_VERSION_V1;
    write_meta(&dir, &header)?;
    eprintln!("page_migrated_v2_to_v3 page_id={page_id}");
    Ok(true)
}

/// Idempotent v3→v4 migration: rewrite only `page.meta` (tmp+fsync+rename), converting the
/// sealed fixed-width render profile v1 into `sealed_v2_pressure(same ink)`. Never touches
/// `background.raw`, `strokes.jsonl`, or `edits.jsonl`. Already-v4+ pages return `Ok(false)`.
pub fn migrate_page_v3_to_v4_if_needed(data_dir: &Path, page_id: &str) -> Result<bool, String> {
    let dir = page_dir(data_dir, page_id);
    let meta_p = meta_path(&dir);
    if !meta_p.exists() {
        return Err(format!(
            "REFUSE PAGE OPEN: missing page.meta in {}",
            dir.display()
        ));
    }
    let meta_s = fs::read_to_string(&meta_p).map_err(|e| e.to_string())?;
    let mut header: PersistedPageHeader = serde_json::from_str(&meta_s)
        .map_err(|e| format!("REFUSE PAGE OPEN: bad page.meta: {e}"))?;
    if header.v >= FORMAT_VERSION_V4 {
        return Ok(false);
    }
    if header.v != FORMAT_VERSION_V3 {
        return Err(format!(
            "REFUSE PAGE OPEN: v3→v4 migration requires v{FORMAT_VERSION_V3} (got {})",
            header.v
        ));
    }
    // Convert the sealed v1 (fixed) profile → sealed v2 pressure of the same ink.
    let v1: PersistedRenderProfileV1 = serde_json::from_value(header.render.clone())
        .map_err(|e| format!("REFUSE PAGE OPEN: bad v3 render profile: {e}"))?;
    let old = validate_persisted_render_profile_v1(&v1)?;
    let pressure = RenderProfile::sealed_v2_pressure(old.ink_byte)?;
    header.render = render_to_value(&pressure)?;
    header.v = FORMAT_VERSION_V4;
    // v4 contract seals edit-journal v1 (deletes only) until v4→v5.
    header.edit_journal_version = EDIT_JOURNAL_VERSION_V1;
    let _admit = crate::sync_verified::note_mutation_from_page_dir(&dir)?;
    write_meta(&dir, &header)?;
    eprintln!("page_migrated_v3_to_v4 page_id={page_id}");
    Ok(true)
}

/// Idempotent v4→v5 migration: rewrite only `page.meta` to seal edit-journal v2
/// (`replace_strokes`). Never touches `background.raw`, `strokes.jsonl`, or `edits.jsonl`.
///
/// Refuses (without rewriting meta) if the source header does not satisfy the v4 contract.
/// Historical F6 v4 pages that omit `pressure_missing_fallback_radius` are normalized only
/// for this migration; the resulting v5 header always seals the field explicitly.
pub fn migrate_page_v4_to_v5_if_needed(data_dir: &Path, page_id: &str) -> Result<bool, String> {
    let dir = page_dir(data_dir, page_id);
    let meta_p = meta_path(&dir);
    if !meta_p.exists() {
        return Err(format!(
            "REFUSE PAGE OPEN: missing page.meta in {}",
            dir.display()
        ));
    }
    let meta_s = fs::read_to_string(&meta_p).map_err(|e| e.to_string())?;
    let mut header: PersistedPageHeader = serde_json::from_str(&meta_s)
        .map_err(|e| format!("REFUSE PAGE OPEN: bad page.meta: {e}"))?;
    if header.v == FORMAT_VERSION {
        return Ok(false);
    }
    if header.v != FORMAT_VERSION_V4 {
        return Err(format!(
            "REFUSE PAGE OPEN: v4→v5 migration requires v{FORMAT_VERSION_V4} (got {})",
            header.v
        ));
    }
    let sealed_render = validate_v4_source_header(&dir, &header)?;
    header.v = FORMAT_VERSION;
    header.edit_journal_version = EDIT_JOURNAL_VERSION;
    header.render = sealed_render;
    let _admit = crate::sync_verified::note_mutation_from_page_dir(&dir)?;
    write_meta(&dir, &header)?;
    eprintln!("page_migrated_v4_to_v5 page_id={page_id}");
    Ok(true)
}

/// Migration-only: normalize a historical F6 v4 `render` value for strict validation.
///
/// If `profile_version == 2` and `pressure_missing_fallback_radius` is **absent**, inject
/// [`LEGACY_FIXED_RADIUS`] (2) into a temporary clone — matching the pre-seal F6 runtime
/// semantic. A **present** value is left untouched (valid `2` passes; anything else fails
/// the subsequent strict parser). Does not mutate durable metadata.
pub fn normalize_historical_v4_render_for_migration(
    render: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let version_val = render
        .get("profile_version")
        .ok_or_else(|| "REFUSE PAGE OPEN: render profile missing profile_version".to_string())?;
    let version = version_val
        .as_u64()
        .ok_or_else(|| "REFUSE PAGE OPEN: render profile_version must be an integer".to_string())?;
    let version = u32::try_from(version)
        .map_err(|_| "REFUSE PAGE OPEN: render profile_version out of range".to_string())?;

    let mut normalized = render.clone();
    if version == RENDER_PROFILE_VERSION
        && normalized.get("pressure_missing_fallback_radius").is_none()
    {
        let obj = normalized
            .as_object_mut()
            .ok_or_else(|| "REFUSE PAGE OPEN: render profile must be a JSON object".to_string())?;
        obj.insert(
            "pressure_missing_fallback_radius".into(),
            serde_json::json!(LEGACY_FIXED_RADIUS),
        );
    }
    Ok(normalized)
}

/// Require a well-formed v4 source page before rewriting meta to v5.
///
/// Returns the sealed render value that must be written into the v5 header (always includes
/// an explicit `pressure_missing_fallback_radius` when profile v2).
fn validate_v4_source_header(
    dir: &Path,
    header: &PersistedPageHeader,
) -> Result<serde_json::Value, String> {
    if header.v != FORMAT_VERSION_V4 {
        return Err(format!(
            "REFUSE PAGE OPEN: v4 source requires format_version {FORMAT_VERSION_V4} (got {})",
            header.v
        ));
    }
    if header.edit_journal_version != EDIT_JOURNAL_VERSION_V1 {
        return Err(format!(
            "REFUSE PAGE OPEN: v4 source requires edit_journal_version {EDIT_JOURNAL_VERSION_V1} (got {})",
            header.edit_journal_version
        ));
    }
    if (header.canonical_width - CANONICAL_WIDTH).abs() > f32::EPSILON
        || (header.canonical_height - CANONICAL_HEIGHT).abs() > f32::EPSILON
    {
        return Err(format!(
            "REFUSE PAGE OPEN: wrong page geometry {}x{} (need {CANONICAL_WIDTH}x{CANONICAL_HEIGHT})",
            header.canonical_width, header.canonical_height
        ));
    }
    if header.bg_layout_version != BG_LAYOUT_VERSION {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported bg_layout_version {}",
            header.bg_layout_version
        ));
    }
    let sealed_render = normalize_historical_v4_render_for_migration(&header.render)?;
    let profile = parse_header_render(&sealed_render)?;
    if profile.pressure.is_none() {
        return Err("REFUSE PAGE OPEN: v4 source requires pressure-capable render profile".into());
    }
    if !edits_path(dir).exists() {
        return Err(format!(
            "REFUSE PAGE OPEN: v4 source missing edits.jsonl in {}",
            dir.display()
        ));
    }
    Ok(sealed_render)
}

/// Bring one page fully current: v2→v3→v4→v5.
pub fn migrate_page_to_current(data_dir: &Path, page_id: &str) -> Result<(), String> {
    migrate_page_v2_to_v3_if_needed(data_dir, page_id)?;
    migrate_page_v3_to_v4_if_needed(data_dir, page_id)?;
    migrate_page_v4_to_v5_if_needed(data_dir, page_id)?;
    Ok(())
}

/// Migrate every manifest-referenced page to the current format (v2→v3→v4→v5).
/// Refuse the notebook if any page fails.
pub fn migrate_all_pages(data_dir: &Path, page_ids: &[String]) -> Result<(), String> {
    for id in page_ids {
        migrate_page_to_current(data_dir, id)
            .map_err(|e| format!("REFUSE NOTEBOOK OPEN: page {id} migration failed: {e}"))?;
    }
    Ok(())
}

pub fn load_existing_page(
    data_dir: &Path,
    page_id: &str,
) -> Result<(FsPageStore, LoadedPage), String> {
    // Ensure current format (v2→v3→v4→v5) before validating/opening.
    migrate_page_to_current(data_dir, page_id)?;
    let dir = page_dir(data_dir, page_id);
    let meta_p = meta_path(&dir);
    let bg_p = bg_path(&dir);
    let j_p = journal_path(&dir);
    let e_p = edits_path(&dir);
    if !meta_p.exists() {
        return Err(format!(
            "REFUSE PAGE OPEN: missing page.meta in {}",
            dir.display()
        ));
    }
    if !bg_p.exists() {
        return Err(format!(
            "REFUSE PAGE OPEN: missing background.raw in {}",
            dir.display()
        ));
    }
    if !j_p.exists() {
        return Err(format!(
            "REFUSE PAGE OPEN: missing strokes.jsonl in {}",
            dir.display()
        ));
    }
    if !e_p.exists() {
        return Err(format!(
            "REFUSE PAGE OPEN: missing edits.jsonl in {}",
            dir.display()
        ));
    }
    let meta_s = fs::read_to_string(&meta_p).map_err(|e| e.to_string())?;
    let header: PersistedPageHeader = serde_json::from_str(&meta_s)
        .map_err(|e| format!("REFUSE PAGE OPEN: bad page.meta: {e}"))?;
    let render_profile = validate_header(&header)?;
    let background = fs::read(&bg_p).map_err(|e| e.to_string())?;
    if background.len() as u64 != header.background_bytes {
        return Err(format!(
            "REFUSE PAGE OPEN: background byte count {} != meta {}",
            background.len(),
            header.background_bytes
        ));
    }
    let sum = rect_checksum(&background);
    if sum != header.background_checksum {
        return Err(format!(
            "REFUSE PAGE OPEN: background checksum {sum:#x} != meta {:#x}",
            header.background_checksum
        ));
    }
    let raw = fs::read(&j_p).map_err(|e| e.to_string())?;
    let (canonical_strokes, truncated_stroke_tail) = parse_stroke_journal(&raw)?;
    let mut writable = true;
    if truncated_stroke_tail {
        let keep = verified_journal_prefix(&raw);
        eprintln!(
            "warning: truncating journal to last verified boundary ({} bytes → {} bytes)",
            raw.len(),
            keep.len()
        );
        {
            let f = OpenOptions::new()
                .write(true)
                .open(&j_p)
                .map_err(|e| e.to_string())?;
            f.set_len(keep.len() as u64).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        writable = true;
    }
    let known_ids: HashSet<String> = canonical_strokes
        .iter()
        .map(|s| s.id.as_str().to_string())
        .collect();
    let edit_raw = fs::read(&e_p).map_err(|e| e.to_string())?;
    let (visible, truncated_edit_tail) = replay_edit_journal(canonical_strokes, &edit_raw)?;
    let truncated_tail = truncated_stroke_tail || truncated_edit_tail;
    if truncated_edit_tail {
        let keep = verified_journal_prefix(&edit_raw);
        eprintln!(
            "warning: truncating edit journal to last verified boundary ({} bytes → {} bytes)",
            edit_raw.len(),
            keep.len()
        );
        {
            let f = OpenOptions::new()
                .write(true)
                .open(&e_p)
                .map_err(|e| e.to_string())?;
            f.set_len(keep.len() as u64).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        writable = true;
    }
    let _ = known_ids; // replaced by visible.known_ids
    let journal = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&j_p)
        .map_err(|e| e.to_string())?;
    let edits = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&e_p)
        .map_err(|e| e.to_string())?;
    let store = FsPageStore {
        dir,
        header: header.clone(),
        journal,
        edits,
        stroke_count: visible.known_ids.len(),
        known_ids: visible.known_ids.clone(),
        deleted_ids: HashSet::new(),
        writable,
    };
    Ok((
        store,
        LoadedPage {
            header,
            background,
            strokes: visible.strokes,
            root_of: visible.root_of,
            truncated_tail,
            page_open: PageOpenKind::Existing,
            render_profile,
        },
    ))
}

/// A page as read by [`load_page_read_only`].
#[derive(Clone, Debug)]
pub struct ReadOnlyPage {
    pub header: PersistedPageHeader,
    pub background: Vec<u8>,
    /// Visible strokes/fragments in render order.
    pub strokes: Vec<Stroke>,
    /// Visible stroke id → authored lineage root id.
    pub root_of: std::collections::HashMap<String, String>,
    pub render_profile: RenderProfile,
    /// An incomplete final journal record was ignored (and left untouched on disk).
    pub truncated_tail: bool,
}

/// Load a page's committed state **without any write**: unlike [`load_existing_page`] it never
/// migrates an older format (it refuses instead), never truncates an incomplete journal tail
/// (the tail is ignored exactly as normal replay ignores it), and opens no append handles.
pub fn load_page_read_only(data_dir: &Path, page_id: &str) -> Result<ReadOnlyPage, String> {
    let dir = page_dir(data_dir, page_id);
    let meta_s = fs::read_to_string(meta_path(&dir))
        .map_err(|e| format!("REFUSE PAGE READ: {}: {e}", meta_path(&dir).display()))?;
    let header: PersistedPageHeader = serde_json::from_str(&meta_s)
        .map_err(|e| format!("REFUSE PAGE READ: bad page.meta in {}: {e}", dir.display()))?;
    if header.v != FORMAT_VERSION {
        return Err(format!(
            "REFUSE PAGE READ: {} is page format v{} (need v{FORMAT_VERSION}); open it in Scribe once to migrate",
            dir.display(),
            header.v
        ));
    }
    let render_profile = validate_header(&header)?;
    let background = fs::read(bg_path(&dir)).map_err(|e| e.to_string())?;
    if background.len() as u64 != header.background_bytes {
        return Err(format!(
            "REFUSE PAGE READ: background byte count {} != meta {} in {}",
            background.len(),
            header.background_bytes,
            dir.display()
        ));
    }
    let sum = rect_checksum(&background);
    if sum != header.background_checksum {
        return Err(format!(
            "REFUSE PAGE READ: background checksum {sum:#x} != meta {:#x} in {}",
            header.background_checksum,
            dir.display()
        ));
    }
    let raw = fs::read(journal_path(&dir)).map_err(|e| e.to_string())?;
    let (base, stroke_tail) = parse_stroke_journal(&raw)?;
    let edit_raw = fs::read(edits_path(&dir)).map_err(|e| e.to_string())?;
    let (visible, edit_tail) = replay_edit_journal(base, &edit_raw)?;
    Ok(ReadOnlyPage {
        header,
        background,
        strokes: visible.strokes,
        root_of: visible.root_of,
        render_profile,
        truncated_tail: stroke_tail || edit_tail,
    })
}

/// Page journals + header without reading `background.raw`.
#[derive(Clone, Debug)]
pub struct PageJournals {
    pub header: PersistedPageHeader,
    pub strokes: Vec<Stroke>,
    pub root_of: std::collections::HashMap<String, String>,
    pub render_profile: RenderProfile,
    pub truncated_tail: bool,
}

/// Load committed strokes/edits without touching `background.raw`.
pub fn load_page_journals_read_only(
    data_dir: &Path,
    page_id: &str,
) -> Result<PageJournals, String> {
    let dir = page_dir(data_dir, page_id);
    let meta_s = fs::read_to_string(meta_path(&dir))
        .map_err(|e| format!("REFUSE PAGE READ: {}: {e}", meta_path(&dir).display()))?;
    let header: PersistedPageHeader = serde_json::from_str(&meta_s)
        .map_err(|e| format!("REFUSE PAGE READ: bad page.meta in {}: {e}", dir.display()))?;
    if header.v != FORMAT_VERSION {
        return Err(format!(
            "REFUSE PAGE READ: {} is page format v{} (need v{FORMAT_VERSION}); open it in Scribe once to migrate",
            dir.display(),
            header.v
        ));
    }
    let render_profile = validate_header(&header)?;
    let raw = fs::read(journal_path(&dir)).map_err(|e| e.to_string())?;
    let (base, stroke_tail) = parse_stroke_journal(&raw)?;
    let edit_raw = fs::read(edits_path(&dir)).map_err(|e| e.to_string())?;
    let (visible, edit_tail) = replay_edit_journal(base, &edit_raw)?;
    Ok(PageJournals {
        header,
        strokes: visible.strokes,
        root_of: visible.root_of,
        render_profile,
        truncated_tail: stroke_tail || edit_tail,
    })
}

fn identity_to_background_ref(
    id: &BackgroundIdentityV1,
    header: &PersistedPageHeader,
    file_len: u64,
) -> Result<BackgroundRef, String> {
    id.validate()?;
    if file_len != header.background_bytes {
        return Err(format!(
            "background file size {file_len} != meta {}",
            header.background_bytes
        ));
    }
    match id {
        BackgroundIdentityV1::Blank { .. } => Ok(BackgroundRef::Blank),
        BackgroundIdentityV1::Blob { sha256, .. } => {
            let sha = Sha256Hex::parse(sha256).map_err(|e| e.to_string())?;
            let w = header.panel_width;
            let h = header.panel_height;
            let stride = if file_len == u64::from(w) * u64::from(h) {
                w
            } else if file_len == u64::from(header.line_length) * u64::from(h) {
                header.line_length
            } else {
                return Err(format!(
                    "background of {file_len} bytes matches neither {w}x{h} nor stride {}",
                    header.line_length
                ));
            };
            Ok(BackgroundRef::Blob {
                sha256: sha,
                byte_len: file_len,
                format: BackgroundFormat {
                    encoding: BackgroundEncoding::Gray8,
                    width: w,
                    height: h,
                    stride,
                },
            })
        }
    }
}

/// Sync identity of `background.raw`: hit the persisted record when valid, otherwise read,
/// verify checksum, hash once, and publish the identity (in-place migration).
pub fn resolve_background_ref(
    dir: &Path,
    header: &mut PersistedPageHeader,
) -> Result<BackgroundRef, String> {
    let bg_p = bg_path(dir);
    let file_len = fs::metadata(&bg_p)
        .map_err(|e| format!("REFUSE PAGE READ: {}: {e}", bg_p.display()))?
        .len();
    if let Some(id) = header.background_identity.clone() {
        match identity_to_background_ref(&id, header, file_len) {
            Ok(r) => {
                bg_io_mut(|s| {
                    s.identity_hits += 1;
                    s.distinct_identities.insert(id.stats_key());
                });
                return Ok(r);
            }
            Err(e) => {
                eprintln!(
                    "background_identity_invalid page={} detail={e}; reconstructing",
                    header.page_id
                );
            }
        }
    }
    let t_read = Instant::now();
    let background = fs::read(&bg_p).map_err(|e| e.to_string())?;
    let read_us = t_read.elapsed().as_micros() as u64;
    bg_io_mut(|s| {
        s.files_read += 1;
        s.bytes_read += background.len() as u64;
        s.read_us += read_us;
    });
    if background.len() as u64 != header.background_bytes {
        return Err(format!(
            "REFUSE PAGE READ: background byte count {} != meta {} in {}",
            background.len(),
            header.background_bytes,
            dir.display()
        ));
    }
    let sum = rect_checksum(&background);
    if sum != header.background_checksum {
        return Err(format!(
            "REFUSE PAGE READ: background checksum {sum:#x} != meta {:#x} in {}",
            header.background_checksum,
            dir.display()
        ));
    }
    let t_hash = Instant::now();
    let blank = background.iter().all(|&b| b == 0xFF);
    let id = if blank {
        BackgroundIdentityV1::Blank {
            v: BACKGROUND_IDENTITY_VERSION,
        }
    } else {
        let sha = sha256_hex(&background);
        bg_io_mut(|s| {
            s.sha256_ops += 1;
            s.bytes_hashed += background.len() as u64;
        });
        BackgroundIdentityV1::Blob {
            v: BACKGROUND_IDENTITY_VERSION,
            sha256: sha.to_string(),
        }
    };
    let hash_us = t_hash.elapsed().as_micros() as u64;
    bg_io_mut(|s| {
        s.hash_us += hash_us;
        s.migrated += 1;
        s.distinct_identities.insert(id.stats_key());
    });
    header.background_identity = Some(id.clone());
    write_meta(dir, header)?;
    eprintln!(
        "background_identity_migrated page={} bytes={}",
        header.page_id,
        background.len()
    );
    identity_to_background_ref(&id, header, file_len)
}

/// Same reconstruction as [`resolve_background_ref`] but never writes `page.meta`.
/// Used by off-device rehearsal against immutable acquisitions.
pub fn resolve_background_ref_read_only(
    dir: &Path,
    header: &PersistedPageHeader,
) -> Result<BackgroundRef, String> {
    let bg_p = bg_path(dir);
    let file_len = fs::metadata(&bg_p)
        .map_err(|e| format!("REFUSE PAGE READ: {}: {e}", bg_p.display()))?
        .len();
    if let Some(id) = header.background_identity.as_ref() {
        if let Ok(r) = identity_to_background_ref(id, header, file_len) {
            return Ok(r);
        }
    }
    let background = fs::read(&bg_p).map_err(|e| e.to_string())?;
    if background.len() as u64 != header.background_bytes {
        return Err(format!(
            "REFUSE PAGE READ: background byte count {} != meta {} in {}",
            background.len(),
            header.background_bytes,
            dir.display()
        ));
    }
    let sum = rect_checksum(&background);
    if sum != header.background_checksum {
        return Err(format!(
            "REFUSE PAGE READ: background checksum {sum:#x} != meta {:#x} in {}",
            header.background_checksum,
            dir.display()
        ));
    }
    let id = if background.iter().all(|&b| b == 0xFF) {
        BackgroundIdentityV1::Blank {
            v: BACKGROUND_IDENTITY_VERSION,
        }
    } else {
        BackgroundIdentityV1::Blob {
            v: BACKGROUND_IDENTITY_VERSION,
            sha256: sha256_hex(&background).to_string(),
        }
    };
    identity_to_background_ref(&id, header, file_len)
}

/// Test helper: strip a published identity so the next resolve reconstructs.
#[cfg(test)]
pub fn strip_background_identity_for_test(dir: &Path) -> Result<(), String> {
    let meta_s = fs::read_to_string(meta_path(dir)).map_err(|e| e.to_string())?;
    let mut header: PersistedPageHeader =
        serde_json::from_str(&meta_s).map_err(|e| e.to_string())?;
    header.background_identity = None;
    write_meta(dir, &header)
}

#[cfg(test)]
pub fn page_meta_path_for_test(dir: &Path) -> PathBuf {
    meta_path(dir)
}

#[cfg(test)]
pub fn background_path_for_test(dir: &Path) -> PathBuf {
    bg_path(dir)
}

/// Bytes of complete newline-terminated records only.
pub fn verified_journal_prefix(raw: &[u8]) -> Vec<u8> {
    if let Some(pos) = raw.iter().rposition(|&b| b == b'\n') {
        raw[..=pos].to_vec()
    } else {
        Vec::new()
    }
}

impl FsPageStore {
    /// Every stroke/fragment ID this page's journals have ever contained (visible or not).
    /// Sync S2C uses it so remote materialization never reuses a local ID.
    pub fn ids_ever_used(&self) -> &HashSet<String> {
        &self.known_ids
    }
}

pub fn page_exists(data_dir: &Path, page_id: &str) -> bool {
    meta_path(&page_dir(data_dir, page_id)).exists()
}

impl PageStore for FsPageStore {
    fn append_committed_stroke(&mut self, stroke: &Stroke) -> Result<PersistTiming, String> {
        if !self.writable {
            return Err("store not writable".into());
        }
        if self.header.page_id.is_empty() {
            return Err("missing page id".into());
        }
        // Invalidate verified stamp before this stroke can become durable.
        let _admit = crate::sync_verified::note_mutation_from_page_dir(&self.dir)?;
        let line = stroke_record_line(stroke)?;
        let t0 = Instant::now();
        self.journal
            .write_all(line.as_bytes())
            .map_err(|e| format!("journal write in {}: {e}", self.dir.display()))?;
        self.journal
            .flush()
            .map_err(|e| format!("journal flush in {}: {e}", self.dir.display()))?;
        let append_us = t0.elapsed().as_micros() as u64;
        let t1 = Instant::now();
        self.journal
            .sync_all()
            .map_err(|e| format!("journal fsync in {}: {e}", self.dir.display()))?;
        let fsync_us = t1.elapsed().as_micros() as u64;
        self.known_ids.insert(stroke.id.as_str().to_string());
        self.stroke_count += 1;
        Ok(PersistTiming {
            append_us,
            fsync_us,
        })
    }

    fn append_delete_stroke(&mut self, stroke_id: &StrokeId) -> Result<PersistTiming, String> {
        if !self.writable {
            return Err("store not writable".into());
        }
        let id = stroke_id.as_str().to_string();
        if !self.known_ids.contains(&id) {
            return Err(format!("REFUSE: delete unknown stroke id {id}"));
        }
        if self.deleted_ids.contains(&id) {
            return Err(format!("REFUSE: duplicate delete stroke id {id}"));
        }
        let _admit = crate::sync_verified::note_mutation_from_page_dir(&self.dir)?;
        let rec = PersistedDeleteStrokeV1::new(stroke_id);
        let mut line = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
        line.push('\n');
        let t0 = Instant::now();
        self.edits
            .write_all(line.as_bytes())
            .map_err(|e| format!("edits write in {}: {e}", self.dir.display()))?;
        self.edits
            .flush()
            .map_err(|e| format!("edits flush in {}: {e}", self.dir.display()))?;
        let append_us = t0.elapsed().as_micros() as u64;
        let t1 = Instant::now();
        self.edits
            .sync_all()
            .map_err(|e| format!("edits fsync in {}: {e}", self.dir.display()))?;
        let fsync_us = t1.elapsed().as_micros() as u64;
        self.deleted_ids.insert(id);
        self.stroke_count = self.stroke_count.saturating_sub(1);
        Ok(PersistTiming {
            append_us,
            fsync_us,
        })
    }

    fn append_replace_strokes(
        &mut self,
        replacements: &[PersistedReplacementV2],
    ) -> Result<PersistTiming, String> {
        if !self.writable {
            return Err("store not writable".into());
        }
        // Structural validation before any durable write. Visibility/collision
        // preflight is the session's responsibility (prepare → append → commit).
        validate_replace_strokes_structural(replacements)?;
        let _admit = crate::sync_verified::note_mutation_from_page_dir(&self.dir)?;
        let rec = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: replacements.to_vec(),
        };
        rec.validate_shape()?;
        let mut line = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
        line.push('\n');
        let t0 = Instant::now();
        self.edits
            .write_all(line.as_bytes())
            .map_err(|e| format!("edits write in {}: {e}", self.dir.display()))?;
        self.edits
            .flush()
            .map_err(|e| format!("edits flush in {}: {e}", self.dir.display()))?;
        let append_us = t0.elapsed().as_micros() as u64;
        let t1 = Instant::now();
        self.edits
            .sync_all()
            .map_err(|e| format!("edits fsync in {}: {e}", self.dir.display()))?;
        let fsync_us = t1.elapsed().as_micros() as u64;
        for rep in replacements {
            for frag in &rep.fragments {
                self.known_ids.insert(frag.id.clone());
            }
        }
        self.stroke_count = self.known_ids.len();
        Ok(PersistTiming {
            append_us,
            fsync_us,
        })
    }

    fn is_writable(&self) -> bool {
        self.writable
    }

    fn stroke_count(&self) -> usize {
        self.stroke_count
    }

    fn has_authored_history(&self) -> bool {
        !self.known_ids.is_empty()
    }
}

impl FsPageStore {
    #[allow(dead_code)]
    pub fn header(&self) -> &PersistedPageHeader {
        &self.header
    }

    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Bulk import: append many committed strokes with ONE fsync (never the pen path). A crash
    /// mid-write leaves at most a torn final line, which newline framing discards on load.
    pub fn append_committed_strokes(&mut self, strokes: &[Stroke]) -> Result<(), String> {
        if !self.writable {
            return Err("store not writable".into());
        }
        if !strokes.is_empty() {
            let _admit = crate::sync_verified::note_mutation_from_page_dir(&self.dir)?;
        }
        let mut buf = String::new();
        for s in strokes {
            if self.known_ids.contains(s.id.as_str()) {
                return Err(format!("duplicate stroke id {}", s.id.as_str()));
            }
            buf.push_str(&stroke_record_line(s)?);
        }
        self.journal
            .write_all(buf.as_bytes())
            .and_then(|()| self.journal.flush())
            .and_then(|()| self.journal.sync_all())
            .map_err(|e| format!("journal bulk append in {}: {e}", self.dir.display()))?;
        for s in strokes {
            self.known_ids.insert(s.id.as_str().to_string());
        }
        self.stroke_count += strokes.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::{ink_for_new_page, resolve_existing_ink, InkRequest};
    use crate::profile::{
        LEGACY_FIXED_RADIUS, RASTERIZER_ID_V2_PRESSURE, RENDER_PROFILE_VERSION, TRANSFORM_ID_V1,
    };
    use notebook_core::new_stroke_id;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Mutate a scalar field inside the header `render` JSON value.
    fn set_render_field(h: &mut PersistedPageHeader, key: &str, val: serde_json::Value) {
        if let serde_json::Value::Object(map) = &mut h.render {
            map.insert(key.to_string(), val);
        } else {
            panic!("render is not an object");
        }
    }

    /// Read a scalar field from the header `render` JSON value.
    fn render_field(h: &PersistedPageHeader, key: &str) -> serde_json::Value {
        h.render
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    fn tmp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-f1-{n}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn profile00() -> RenderProfile {
        RenderProfile::sealed_v2_pressure(0x00).unwrap()
    }

    fn profile_ff() -> RenderProfile {
        RenderProfile::sealed_v2_pressure(0xff).unwrap()
    }

    fn stroke_with(id: &str, pts: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: pts.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            brush: None,
            finished: true,
        }
    }

    fn geo(w: u32, h: u32, stride: u32) -> FbGeometry {
        FbGeometry {
            xres: w,
            yres: h,
            xres_virtual: w,
            yres_virtual: h,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: stride,
            smem_len: stride.saturating_mul(h),
        }
    }

    #[test]
    fn texts_jsonl_round_trips_and_missing_is_empty() {
        let dir = tmp_dir();
        assert!(load_page_texts(&dir).unwrap().is_empty());
        let t = sync_engine::testkit::text_obj(1, "  keep spaces\nand lines  ");
        write_page_texts(&dir, std::slice::from_ref(&t)).unwrap();
        assert_eq!(load_page_texts(&dir).unwrap(), vec![t]);
        write_page_texts(&dir, &[]).unwrap();
        assert!(!texts_path(&dir).exists());
        assert!(load_page_texts(&dir).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn texts_jsonl_preserves_order_and_ids() {
        let dir = tmp_dir();
        let a = sync_engine::testkit::text_obj(1, "first");
        let b = sync_engine::testkit::text_obj(2, "second");
        write_page_texts(&dir, &[a.clone(), b.clone()]).unwrap();
        let got = load_page_texts(&dir).unwrap();
        assert_eq!(got[0].text_id, a.text_id);
        assert_eq!(got[1].text_id, b.text_id);
        assert_eq!(got, vec![a, b]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn texts_jsonl_replace_nonempty_a_then_b() {
        let dir = tmp_dir();
        let a = sync_engine::testkit::text_obj(1, "alpha");
        let mut b = sync_engine::testkit::text_obj(1, "bravo");
        b.x = 400.0;
        write_page_texts(&dir, &[a]).unwrap();
        write_page_texts(&dir, &[b.clone()]).unwrap();
        let got = load_page_texts(&dir).unwrap();
        assert_eq!(got, vec![b]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_stroke_dto() {
        let s = stroke_with("s-1", &[(1.0, 2.0), (3.0, 4.0)]);
        let p = PersistedStrokeV1::from_stroke(&s).unwrap();
        assert_eq!(p.v, STROKE_RECORD_VERSION);
        let back = p.to_stroke().unwrap();
        assert_eq!(back.id.as_str(), "s-1");
        assert_eq!(back.points.len(), 2);
        assert!(back.finished);
    }

    #[test]
    fn v2_stroke_round_trip_and_style_required() {
        let mut s = stroke_with("s-p", &[(1.0, 2.0), (3.0, 4.0)]);
        s.brush = Some(STYLE_PRESSURE_WIDTH_V1.to_string());
        let p = PersistedStrokeV2::from_stroke(&s).unwrap();
        assert_eq!(p.v, STROKE_RECORD_VERSION_V2);
        assert_eq!(p.style, STYLE_PRESSURE_WIDTH_V1);
        let back = p.to_stroke().unwrap();
        assert_eq!(back.brush.as_deref(), Some(STYLE_PRESSURE_WIDTH_V1));
        // Non-pressure brush must not serialize as v2.
        let legacy = stroke_with("s-l", &[(0.0, 0.0)]);
        assert!(PersistedStrokeV2::from_stroke(&legacy).is_err());
    }

    #[test]
    fn mixed_v1_v2_journal_loads_both() {
        let mut v2 = stroke_with("s-press", &[(1.0, 1.0), (2.0, 2.0)]);
        v2.brush = Some(STYLE_PRESSURE_WIDTH_V1.to_string());
        let l1 = stroke_record_line(&stroke_with("s-legacy", &[(0.0, 0.0)])).unwrap();
        let l2 = stroke_record_line(&v2).unwrap();
        let raw = format!("{l1}{l2}");
        let (strokes, trunc) = parse_stroke_journal(raw.as_bytes()).unwrap();
        assert!(!trunc);
        assert_eq!(strokes.len(), 2);
        assert!(strokes[0].brush.is_none());
        assert_eq!(strokes[1].brush.as_deref(), Some(STYLE_PRESSURE_WIDTH_V1));
    }

    #[test]
    fn unknown_stroke_record_version_refuses() {
        let raw = b"{\"v\":9,\"type\":\"stroke\",\"id\":\"x\",\"finished\":true,\"points\":[{\"x\":0.0,\"y\":0.0}]}\n";
        assert!(parse_stroke_journal(raw).is_err());
    }

    #[test]
    fn refuse_unfinished_persist() {
        let s = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(0.0, 0.0)],
            brush: None,
            finished: false,
        };
        assert!(PersistedStrokeV1::from_stroke(&s).is_err());
    }

    #[test]
    fn truncated_tail_loads_prior() {
        let a =
            PersistedStrokeV1::from_stroke(&stroke_with("s-a", &[(0.0, 0.0), (1.0, 1.0)])).unwrap();
        let b =
            PersistedStrokeV1::from_stroke(&stroke_with("s-b", &[(2.0, 2.0), (3.0, 3.0)])).unwrap();
        let mut raw = String::new();
        raw.push_str(&serde_json::to_string(&a).unwrap());
        raw.push('\n');
        raw.push_str(&serde_json::to_string(&b).unwrap());
        raw.push('\n');
        raw.push_str("{\"v\":1,\"type\":\"stroke\",\"id\":\"s-c\",\"finished\":true,\"points\":[");
        let (strokes, trunc) = parse_stroke_journal(raw.as_bytes()).unwrap();
        assert!(trunc);
        assert_eq!(strokes.len(), 2);
        assert_eq!(strokes[0].id.as_str(), "s-a");
        assert_eq!(strokes[1].id.as_str(), "s-b");
    }

    #[test]
    fn newline_framing_rejects_valid_json_without_newline() {
        let a = PersistedStrokeV1::from_stroke(&stroke_with("s-a", &[(0.0, 0.0)])).unwrap();
        let json = serde_json::to_string(&a).unwrap();
        // Valid JSON, no trailing newline → incomplete tail, NOT loaded.
        let (strokes, trunc) = parse_stroke_journal(json.as_bytes()).unwrap();
        assert!(trunc);
        assert!(strokes.is_empty());
        // Newline-terminated → accepted.
        let with_nl = format!("{json}\n");
        let (strokes, trunc) = parse_stroke_journal(with_nl.as_bytes()).unwrap();
        assert!(!trunc);
        assert_eq!(strokes.len(), 1);

        let ids: HashSet<String> = ["s-a".into()].into_iter().collect();
        let del = PersistedDeleteStrokeV1::new(&StrokeId::new("s-a"));
        let ej = serde_json::to_string(&del).unwrap();
        let (deleted, trunc) = parse_edit_journal(ej.as_bytes(), &ids).unwrap();
        assert!(trunc);
        assert!(deleted.is_empty());
        let ej_nl = format!("{ej}\n");
        let (deleted, trunc) = parse_edit_journal(ej_nl.as_bytes(), &ids).unwrap();
        assert!(!trunc);
        assert_eq!(deleted, vec!["s-a".to_string()]);
    }

    #[test]
    fn recovered_on_disk_journal_ends_at_committed_newline() {
        let raw = b"{\"v\":1,\"type\":\"stroke\",\"id\":\"s-a\",\"finished\":true,\"points\":[{\"x\":0.0,\"y\":0.0}]}\n{\"v\":1,\"type\":\"stroke\",\"id\":\"s-b\",\"finished\":true,\"points\":[{\"x\":1.0,\"y\":1.0}]}";
        let keep = verified_journal_prefix(raw);
        assert!(keep.ends_with(b"\n"));
        assert_eq!(keep, &raw[..=raw.iter().position(|&b| b == b'\n').unwrap()]);
        let (strokes, trunc) = parse_stroke_journal(raw).unwrap();
        assert!(trunc);
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].id.as_str(), "s-a");
    }

    #[test]
    fn malformed_middle_fails_closed() {
        let a = PersistedStrokeV1::from_stroke(&stroke_with("s-a", &[(0.0, 0.0)])).unwrap();
        let mut raw = String::new();
        raw.push_str(&serde_json::to_string(&a).unwrap());
        raw.push('\n');
        raw.push_str("{not-json}\n");
        raw.push_str(&serde_json::to_string(&a).unwrap().replace("s-a", "s-b"));
        raw.push('\n');
        assert!(parse_stroke_journal(raw.as_bytes()).is_err());
    }

    #[test]
    fn duplicate_id_fails() {
        let a = PersistedStrokeV1::from_stroke(&stroke_with("s-dup", &[(0.0, 0.0)])).unwrap();
        let line = serde_json::to_string(&a).unwrap();
        let raw = format!("{line}\n{line}\n");
        assert!(parse_stroke_journal(raw.as_bytes())
            .unwrap_err()
            .contains("duplicate"));
    }

    #[test]
    fn create_load_append_round_trip() {
        let root = tmp_dir();
        let bg = vec![0xAAu8; 1860 * 100];
        let (mut store, loaded) =
            create_new_page(&root, "page-1", 1860, 100, 1860, &bg, &profile00()).unwrap();
        assert_eq!(loaded.page_open, PageOpenKind::New);
        assert_eq!(loaded.render_profile.ink_byte, 0x00);
        assert_eq!(loaded.header.v, FORMAT_VERSION);
        assert_eq!(loaded.render_profile.brush_radius, LEGACY_FIXED_RADIUS);
        assert_eq!(
            render_field(&loaded.header, "rasterizer"),
            serde_json::json!(RASTERIZER_ID_V2_PRESSURE)
        );
        assert_eq!(
            render_field(&loaded.header, "transform"),
            serde_json::json!(TRANSFORM_ID_V1)
        );
        assert!(loaded.render_profile.pressure.is_some());
        let s = stroke_with("s-live", &[(10.0, 20.0), (30.0, 40.0)]);
        let t = store.append_committed_stroke(&s).unwrap();
        assert!(t.commit_us() > 0);
        drop(store);
        let (_store2, loaded2) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(loaded2.page_open, PageOpenKind::Existing);
        assert_eq!(loaded2.strokes.len(), 1);
        assert_eq!(loaded2.render_profile.ink_byte, 0x00);
        assert!(loaded2.strokes[0].points[0].x < 1860.0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn new_explicit_ff_persists_ff() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 50 * 10];
        let ink = ink_for_new_page(InkRequest::Explicit(0xff));
        let profile = RenderProfile::sealed_v2_pressure(ink).unwrap();
        let (_s, loaded) = create_new_page(&root, "page-1", 50, 10, 50, &bg, &profile).unwrap();
        assert_eq!(
            render_field(&loaded.header, "ink_byte"),
            serde_json::json!(0xff)
        );
        let (_s2, loaded2) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(
            resolve_existing_ink(InkRequest::Default, loaded2.render_profile.ink_byte).unwrap(),
            0xff
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn existing_omitted_uses_persisted_ink() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 40 * 8];
        let (_s, _) = create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile_ff()).unwrap();
        let (_s2, loaded) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(
            resolve_existing_ink(InkRequest::Default, loaded.render_profile.ink_byte).unwrap(),
            0xff
        );
        assert!(
            resolve_existing_ink(InkRequest::Explicit(0x00), loaded.render_profile.ink_byte)
                .is_err()
        );
        assert!(
            resolve_existing_ink(InkRequest::Explicit(0xff), loaded.render_profile.ink_byte)
                .is_ok()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsupported_renderer_refuses() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 20 * 5];
        let (_s, loaded) = create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile00()).unwrap();
        let mut h = loaded.header.clone();
        set_render_field(&mut h, "rasterizer", serde_json::json!("future-aa"));
        write_meta(&page_dir(&root, "page-1"), &h).unwrap();
        let err = match load_existing_page(&root, "page-1") {
            Ok(_) => panic!("expected refuse"),
            Err(e) => e,
        };
        assert!(err.contains("rasterizer") || err.contains("render profile"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsupported_profile_version_refuses() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 20 * 5];
        let (_s, loaded) = create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile00()).unwrap();
        let mut h = loaded.header.clone();
        set_render_field(&mut h, "profile_version", serde_json::json!(99));
        write_meta(&page_dir(&root, "page-1"), &h).unwrap();
        let err = match load_existing_page(&root, "page-1") {
            Ok(_) => panic!("expected refuse"),
            Err(e) => e,
        };
        assert!(err.contains("profile_version") || err.contains("render profile"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsupported_brush_radius_refuses() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 20 * 5];
        let (_s, loaded) = create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile00()).unwrap();
        let mut h = loaded.header.clone();
        set_render_field(&mut h, "legacy_fixed_radius", serde_json::json!(9));
        write_meta(&page_dir(&root, "page-1"), &h).unwrap();
        let err = match load_existing_page(&root, "page-1") {
            Ok(_) => panic!("expected refuse"),
            Err(e) => e,
        };
        assert!(err.contains("legacy_fixed_radius") || err.contains("render profile"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsupported_transform_refuses() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 20 * 5];
        let (_s, loaded) = create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile00()).unwrap();
        let mut h = loaded.header.clone();
        set_render_field(&mut h, "transform", serde_json::json!("rotated"));
        write_meta(&page_dir(&root, "page-1"), &h).unwrap();
        let err = match load_existing_page(&root, "page-1") {
            Ok(_) => panic!("expected refuse"),
            Err(e) => e,
        };
        assert!(err.contains("transform") || err.contains("render profile"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn framebuffer_geometry_mismatch_refuses() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 50 * 10];
        let (_s, loaded) = create_new_page(&root, "page-1", 50, 10, 50, &bg, &profile00()).unwrap();
        assert!(validate_live_framebuffer(&loaded.header, &geo(50, 10, 50)).is_ok());
        assert!(validate_live_framebuffer(&loaded.header, &geo(51, 10, 50))
            .unwrap_err()
            .contains("framebuffer geometry incompatible"));
        assert!(validate_live_framebuffer(&loaded.header, &geo(50, 11, 50))
            .unwrap_err()
            .contains("framebuffer geometry incompatible"));
        assert!(validate_live_framebuffer(&loaded.header, &geo(50, 10, 64))
            .unwrap_err()
            .contains("framebuffer geometry incompatible"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bad_checksum_refuses() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 50 * 10];
        let (_s, _) = create_new_page(&root, "page-1", 50, 10, 50, &bg, &profile00()).unwrap();
        fs::write(bg_path(&page_dir(&root, "page-1")), vec![0x22u8; 500]).unwrap();
        let err = match load_existing_page(&root, "page-1") {
            Ok(_) => panic!("expected refuse"),
            Err(e) => e,
        };
        assert!(err.contains("checksum") || err.contains("byte count"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsupported_version_refuses() {
        let mut h = PersistedPageHeader {
            v: 99,
            page_id: "page-1".into(),
            canonical_width: CANONICAL_WIDTH,
            canonical_height: CANONICAL_HEIGHT,
            panel_width: 10,
            panel_height: 10,
            line_length: 10,
            background_bytes: 100,
            background_checksum: 0,
            bg_layout_version: BG_LAYOUT_VERSION,
            render: render_to_value(&profile00()).unwrap(),
            edit_journal_version: EDIT_JOURNAL_VERSION,
            background_identity: None,
        };
        assert!(validate_header(&h).is_err());
        h.v = FORMAT_VERSION;
        h.canonical_width = 1.0;
        assert!(validate_header(&h).is_err());
        h.canonical_width = CANONICAL_WIDTH;
        assert!(validate_header(&h).is_ok());
        assert_eq!(
            render_field(&h, "profile_version"),
            serde_json::json!(RENDER_PROFILE_VERSION)
        );
    }

    #[test]
    fn memory_fail_append() {
        let mut m = MemoryPageStore::new();
        m.fail_append = true;
        let s = stroke_with("s-x", &[(1.0, 1.0)]);
        assert!(m.append_committed_stroke(&s).is_err());
    }

    #[test]
    fn verified_prefix_drops_partial() {
        let p = verified_journal_prefix(b"{\"a\":1}\n{\"b\":");
        assert_eq!(p, b"{\"a\":1}\n");
    }

    /// Force an on-disk page down to a legacy v2 layout: v2 header, v1 (fixed) render profile,
    /// no edits.jsonl. Returns nothing; mutates disk in place.
    fn downgrade_to_legacy_v2(dir: &Path, ink: u8) {
        let mut header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(meta_path(dir)).unwrap()).unwrap();
        let v1 = RenderProfile::sealed_v1(ink)
            .unwrap()
            .to_persisted_v1()
            .unwrap();
        header.v = FORMAT_VERSION_V2;
        header.edit_journal_version = 0;
        header.render = serde_json::to_value(&v1).unwrap();
        write_meta(dir, &header).unwrap();
        let _ = fs::remove_file(edits_path(dir));
    }

    #[test]
    fn full_migration_preserves_stroke_and_bg_bytes() {
        let root = tmp_dir();
        let bg = vec![0x55u8; 40 * 8];
        let (mut store, _) =
            create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile00()).unwrap();
        // Legacy fixed stroke record (v1) so migration is byte-identity on strokes.jsonl.
        let s = stroke_with("s-keep", &[(1.0, 2.0), (3.0, 4.0)]);
        store.append_committed_stroke(&s).unwrap();
        drop(store);
        let dir = page_dir(&root, "page-1");
        downgrade_to_legacy_v2(&dir, 0x00);
        let strokes_before = fs::read(journal_path(&dir)).unwrap();
        let bg_before = fs::read(bg_path(&dir)).unwrap();

        // v2→v3
        assert!(migrate_page_v2_to_v3_if_needed(&root, "page-1").unwrap());
        // v3→v4
        assert!(migrate_page_v3_to_v4_if_needed(&root, "page-1").unwrap());
        let edits_before = fs::read(edits_path(&dir)).unwrap();
        // v4→v5
        assert!(migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap());

        // bg + strokes + edits untouched across all migrations.
        assert_eq!(fs::read(journal_path(&dir)).unwrap(), strokes_before);
        assert_eq!(fs::read(bg_path(&dir)).unwrap(), bg_before);
        assert_eq!(fs::read(edits_path(&dir)).unwrap(), edits_before);

        let (_s, loaded) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(loaded.header.v, FORMAT_VERSION);
        assert_eq!(loaded.header.edit_journal_version, EDIT_JOURNAL_VERSION);
        assert_eq!(loaded.strokes.len(), 1);
        // Legacy stroke loads with no brush → renders fixed.
        assert!(loaded.strokes[0].brush.is_none());
        // Profile is now pressure-capable.
        assert!(loaded.render_profile.pressure.is_some());
        assert!(edits_path(&dir).exists());
        // Idempotent.
        assert!(!migrate_page_v2_to_v3_if_needed(&root, "page-1").unwrap());
        assert!(!migrate_page_v3_to_v4_if_needed(&root, "page-1").unwrap());
        assert!(!migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn v3_to_v4_converts_profile_only() {
        let root = tmp_dir();
        let bg = vec![0x33u8; 40 * 8];
        let (mut store, _) =
            create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile_ff()).unwrap();
        store
            .append_committed_stroke(&stroke_with("s-a", &[(1.0, 2.0)]))
            .unwrap();
        drop(store);
        let dir = page_dir(&root, "page-1");
        // Downgrade to a v3 page: v3 header + v1 (fixed) render + present edits.jsonl.
        let mut header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(meta_path(&dir)).unwrap()).unwrap();
        let v1 = RenderProfile::sealed_v1(0xff)
            .unwrap()
            .to_persisted_v1()
            .unwrap();
        header.v = FORMAT_VERSION_V3;
        header.edit_journal_version = EDIT_JOURNAL_VERSION_V1;
        header.render = serde_json::to_value(&v1).unwrap();
        write_meta(&dir, &header).unwrap();
        let strokes_before = fs::read(journal_path(&dir)).unwrap();
        let bg_before = fs::read(bg_path(&dir)).unwrap();
        let edits_before = fs::read(edits_path(&dir)).unwrap();

        assert!(migrate_page_v3_to_v4_if_needed(&root, "page-1").unwrap());

        assert_eq!(fs::read(journal_path(&dir)).unwrap(), strokes_before);
        assert_eq!(fs::read(bg_path(&dir)).unwrap(), bg_before);
        assert_eq!(fs::read(edits_path(&dir)).unwrap(), edits_before);
        let (_s, loaded) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(loaded.header.v, FORMAT_VERSION);
        assert_eq!(loaded.render_profile.ink_byte, 0xff);
        assert!(loaded.render_profile.pressure.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_append_round_trip_and_reload() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 50 * 10];
        let (mut store, _) =
            create_new_page(&root, "page-1", 50, 10, 50, &bg, &profile00()).unwrap();
        store
            .append_committed_stroke(&stroke_with("s-a", &[(0.0, 0.0), (1.0, 1.0)]))
            .unwrap();
        store
            .append_committed_stroke(&stroke_with("s-b", &[(2.0, 2.0), (3.0, 3.0)]))
            .unwrap();
        store.append_delete_stroke(&StrokeId::new("s-a")).unwrap();
        drop(store);
        let (_s, loaded) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(loaded.strokes.len(), 1);
        assert_eq!(loaded.strokes[0].id.as_str(), "s-b");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_and_duplicate_delete_refuse() {
        let known = HashSet::from(["s-a".to_string()]);
        let bad = b"{\"v\":1,\"type\":\"delete_stroke\",\"stroke_id\":\"missing\"}\n";
        assert!(parse_edit_journal(bad, &known)
            .unwrap_err()
            .contains("unknown"));
        let dup = b"{\"v\":1,\"type\":\"delete_stroke\",\"stroke_id\":\"s-a\"}\n{\"v\":1,\"type\":\"delete_stroke\",\"stroke_id\":\"s-a\"}\n";
        assert!(parse_edit_journal(dup, &known)
            .unwrap_err()
            .contains("duplicate"));
    }

    #[test]
    fn truncated_edit_tail_recovers_prior() {
        let known = HashSet::from(["s-a".to_string(), "s-b".to_string()]);
        let mut raw = String::new();
        raw.push_str(
            &serde_json::to_string(&PersistedDeleteStrokeV1::new(&StrokeId::new("s-a"))).unwrap(),
        );
        raw.push('\n');
        raw.push_str("{\"v\":1,\"type\":\"delete_stroke\",\"stroke_id\":\"s-b");
        let (deleted, trunc) = parse_edit_journal(raw.as_bytes(), &known).unwrap();
        assert!(trunc);
        assert_eq!(deleted, vec!["s-a".to_string()]);
    }

    #[test]
    fn malformed_middle_edit_refuses() {
        let known = HashSet::from(["s-a".to_string()]);
        let raw = b"{\"v\":1,\"type\":\"delete_stroke\",\"stroke_id\":\"s-a\"}\n{not-json}\n";
        assert!(parse_edit_journal(raw, &known).is_err());
    }

    #[test]
    fn memory_fail_delete_leaves_strokes() {
        let mut m = MemoryPageStore::new();
        let s = stroke_with("s-x", &[(1.0, 1.0)]);
        m.append_committed_stroke(&s).unwrap();
        m.fail_delete = true;
        assert!(m.append_delete_stroke(&StrokeId::new("s-x")).is_err());
        assert_eq!(m.strokes.len(), 1);
        m.fail_delete = false;
        m.fail_delete_fsync = true;
        assert!(m.append_delete_stroke(&StrokeId::new("s-x")).is_err());
        assert_eq!(m.strokes.len(), 1);
    }

    #[test]
    fn new_v3_page_creates_edits_jsonl() {
        let root = tmp_dir();
        let bg = vec![0x22u8; 20 * 5];
        let (_s, loaded) = create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile00()).unwrap();
        assert_eq!(loaded.header.v, FORMAT_VERSION);
        assert!(edits_path(&page_dir(&root, "page-1")).exists());
        let _ = fs::remove_dir_all(root);
    }

    fn frag(id: &str, pts: &[(f32, f32)]) -> PersistedFragmentV2 {
        PersistedFragmentV2 {
            id: id.into(),
            style: None,
            points: pts
                .iter()
                .map(|(x, y)| PersistedPointV1 {
                    x: *x,
                    y: *y,
                    pressure: None,
                })
                .collect(),
        }
    }

    #[test]
    fn mixed_v1_v2_replay_order_and_lineage() {
        let base = vec![
            stroke_with("s-a", &[(0.0, 0.0), (1.0, 0.0)]),
            stroke_with("s-b", &[(0.0, 1.0), (1.0, 1.0)]),
            stroke_with("s-c", &[(0.0, 2.0), (10.0, 2.0)]),
        ];
        let del = PersistedDeleteStrokeV1::new(&StrokeId::new("s-b"));
        let rep1 = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: vec![PersistedReplacementV2 {
                target_stroke_id: "s-c".into(),
                fragments: vec![
                    frag("s-c1", &[(0.0, 2.0), (4.0, 2.0)]),
                    frag("s-c2", &[(6.0, 2.0), (10.0, 2.0)]),
                ],
            }],
        };
        let rep2 = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: vec![PersistedReplacementV2 {
                target_stroke_id: "s-c2".into(),
                fragments: vec![
                    frag("s-c2a", &[(6.0, 2.0), (7.0, 2.0)]),
                    frag("s-c2b", &[(8.0, 2.0), (10.0, 2.0)]),
                ],
            }],
        };
        let mut raw = String::new();
        raw.push_str(&serde_json::to_string(&del).unwrap());
        raw.push('\n');
        raw.push_str(&serde_json::to_string(&rep1).unwrap());
        raw.push('\n');
        raw.push_str(&serde_json::to_string(&rep2).unwrap());
        raw.push('\n');
        let (model, trunc) = replay_edit_journal(base, raw.as_bytes()).unwrap();
        assert!(!trunc);
        let ids: Vec<_> = model.strokes.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s-a", "s-c1", "s-c2a", "s-c2b"]);
        assert_eq!(model.root_of.get("s-c1").map(String::as_str), Some("s-c"));
        assert_eq!(model.root_of.get("s-c2a").map(String::as_str), Some("s-c"));
        assert_eq!(model.root_of.get("s-c2b").map(String::as_str), Some("s-c"));
    }

    #[test]
    fn replace_unknown_target_refuses() {
        let base = vec![stroke_with("s-a", &[(0.0, 0.0)])];
        let rep = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: vec![PersistedReplacementV2 {
                target_stroke_id: "missing".into(),
                fragments: vec![],
            }],
        };
        let raw = format!("{}\n", serde_json::to_string(&rep).unwrap());
        assert!(replay_edit_journal(base, raw.as_bytes())
            .unwrap_err()
            .contains("unknown"));
    }

    #[test]
    fn replace_duplicate_target_refuses() {
        let base = vec![stroke_with("s-a", &[(0.0, 0.0), (1.0, 0.0)])];
        let rep = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: vec![
                PersistedReplacementV2 {
                    target_stroke_id: "s-a".into(),
                    fragments: vec![frag("f1", &[(0.0, 0.0)])],
                },
                PersistedReplacementV2 {
                    target_stroke_id: "s-a".into(),
                    fragments: vec![],
                },
            ],
        };
        let raw = format!("{}\n", serde_json::to_string(&rep).unwrap());
        assert!(replay_edit_journal(base, raw.as_bytes())
            .unwrap_err()
            .contains("duplicate"));
    }

    #[test]
    fn replace_fragment_id_collisions_refuse() {
        let base = vec![
            stroke_with("s-a", &[(0.0, 0.0), (1.0, 0.0)]),
            stroke_with("s-b", &[(0.0, 1.0), (1.0, 1.0)]),
        ];
        // Collide with base id.
        let rep = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: vec![PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![frag("s-b", &[(0.0, 0.0)])],
            }],
        };
        let raw = format!("{}\n", serde_json::to_string(&rep).unwrap());
        assert!(replay_edit_journal(base.clone(), raw.as_bytes())
            .unwrap_err()
            .contains("collides"));

        // Duplicate fragment ids in same op.
        let rep2 = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: vec![PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![frag("f-x", &[(0.0, 0.0)]), frag("f-x", &[(1.0, 0.0)])],
            }],
        };
        let raw2 = format!("{}\n", serde_json::to_string(&rep2).unwrap());
        assert!(replay_edit_journal(base, raw2.as_bytes()).is_err());
    }

    #[test]
    fn replace_invalid_style_and_pressure_refuse() {
        let base = vec![stroke_with("s-a", &[(0.0, 0.0), (1.0, 0.0)])];
        let bad_style = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: vec![PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![PersistedFragmentV2 {
                    id: "f1".into(),
                    style: Some("nope".into()),
                    points: vec![PersistedPointV1 {
                        x: 0.0,
                        y: 0.0,
                        pressure: None,
                    }],
                }],
            }],
        };
        let raw = format!("{}\n", serde_json::to_string(&bad_style).unwrap());
        assert!(replay_edit_journal(base.clone(), raw.as_bytes())
            .unwrap_err()
            .contains("style"));

        let bad_p = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: vec![PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![PersistedFragmentV2 {
                    id: "f1".into(),
                    style: Some(crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1.into()),
                    points: vec![PersistedPointV1 {
                        x: 0.0,
                        y: 0.0,
                        pressure: Some(2.0),
                    }],
                }],
            }],
        };
        let raw = format!("{}\n", serde_json::to_string(&bad_p).unwrap());
        assert!(replay_edit_journal(base, raw.as_bytes()).is_err());
    }

    #[test]
    fn replace_valid_json_without_newline_not_applied() {
        let base = vec![stroke_with("s-a", &[(0.0, 0.0), (1.0, 0.0)])];
        let rep = PersistedReplaceStrokesV2 {
            v: EDIT_RECORD_VERSION_V2,
            record_type: "replace_strokes".into(),
            replacements: vec![PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![],
            }],
        };
        let json = serde_json::to_string(&rep).unwrap();
        let (model, trunc) = replay_edit_journal(base.clone(), json.as_bytes()).unwrap();
        assert!(trunc);
        assert_eq!(model.strokes.len(), 1);
        assert_eq!(model.strokes[0].id.as_str(), "s-a");
        let with_nl = format!("{json}\n");
        let (model, trunc) = replay_edit_journal(base, with_nl.as_bytes()).unwrap();
        assert!(!trunc);
        assert!(model.strokes.is_empty());
    }

    #[test]
    fn replace_append_round_trip_reload() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 50 * 10];
        let (mut store, _) =
            create_new_page(&root, "page-1", 50, 10, 50, &bg, &profile00()).unwrap();
        store
            .append_committed_stroke(&stroke_with("s-a", &[(0.0, 0.0), (10.0, 0.0)]))
            .unwrap();
        store
            .append_replace_strokes(&[PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![
                    frag("s-a1", &[(0.0, 0.0), (3.0, 0.0)]),
                    frag("s-a2", &[(7.0, 0.0), (10.0, 0.0)]),
                ],
            }])
            .unwrap();
        drop(store);
        let (_s, loaded) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(loaded.strokes.len(), 2);
        assert_eq!(loaded.strokes[0].id.as_str(), "s-a1");
        assert_eq!(loaded.strokes[1].id.as_str(), "s-a2");
        assert_eq!(loaded.root_of.get("s-a1").map(String::as_str), Some("s-a"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn same_id_geometry_rewrite_survives_reload_and_keeps_lineage() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 50 * 10];
        let (mut store, _) =
            create_new_page(&root, "page-1", 50, 10, 50, &bg, &profile00()).unwrap();
        store
            .append_committed_stroke(&stroke_with("s-a", &[(1.0, 2.0), (10.0, 2.0)]))
            .unwrap();
        let page = load_existing_page(&root, "page-1").unwrap().1;
        let prepared = prepare_replace_strokes(
            &VisibleModel {
                strokes: page.strokes,
                root_of: page.root_of,
                known_ids: ["s-a".to_string()].into_iter().collect(),
            },
            &[PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![frag("s-a", &[(101.0, 202.0), (110.0, 202.0)])],
            }],
        )
        .unwrap();
        assert_eq!(prepared.strokes[0].id.as_str(), "s-a");
        store
            .append_replace_strokes(&[PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![frag("s-a", &[(101.0, 202.0), (110.0, 202.0)])],
            }])
            .unwrap();
        drop(store);
        let (_store, loaded) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(loaded.strokes[0].id.as_str(), "s-a");
        assert_eq!(loaded.root_of.get("s-a").map(String::as_str), Some("s-a"));
        assert_eq!(loaded.strokes[0].points[0].x, 101.0);
        assert_eq!(loaded.strokes[0].points[0].y, 202.0);
        let _ = fs::remove_dir_all(root);
    }

    fn forge_valid_v4(dir: &Path) {
        let mut header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(meta_path(dir)).unwrap()).unwrap();
        header.v = FORMAT_VERSION_V4;
        header.edit_journal_version = EDIT_JOURNAL_VERSION_V1;
        // Keep pressure-capable profile from create_new_page.
        write_meta(dir, &header).unwrap();
    }

    #[test]
    fn fragment_coordinates_refuse_non_finite_and_oob() {
        assert!(validate_persisted_canonical_xy(0.0, 0.0).is_ok());
        assert!(validate_persisted_canonical_xy(CANONICAL_WIDTH, CANONICAL_HEIGHT).is_ok());
        assert!(validate_persisted_canonical_xy(f32::NAN, 1.0).is_err());
        assert!(validate_persisted_canonical_xy(1.0, f32::INFINITY).is_err());
        assert!(validate_persisted_canonical_xy(-1.0, 1.0).is_err());
        assert!(validate_persisted_canonical_xy(CANONICAL_WIDTH + 1.0, 1.0).is_err());
        assert!(validate_persisted_canonical_xy(1.0, CANONICAL_HEIGHT + 0.5).is_err());

        let bad = PersistedFragmentV2 {
            id: "f".into(),
            style: None,
            points: vec![PersistedPointV1 {
                x: -1.0,
                y: 0.0,
                pressure: None,
            }],
        };
        assert!(fragment_to_stroke(&bad).is_err());
        assert!(
            validate_replace_strokes_structural(&[PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![bad],
            }])
            .is_err()
        );
    }

    #[test]
    fn explicit_legacy_style_preserved_across_nested_replace() {
        let base = Stroke {
            id: StrokeId::new("s-legacy"),
            points: vec![Point::new(0.0, 0.0), Point::new(100.0, 0.0)],
            brush: Some(STYLE_LEGACY_FIXED_V1.to_string()),
            finished: true,
        };
        let mut model = VisibleModel::from_base_strokes(vec![base]);
        apply_replace_strokes(
            &mut model,
            vec![PersistedReplacementV2 {
                target_stroke_id: "s-legacy".into(),
                fragments: vec![
                    PersistedFragmentV2 {
                        id: "f1".into(),
                        style: Some(STYLE_LEGACY_FIXED_V1.to_string()),
                        points: vec![
                            PersistedPointV1 {
                                x: 0.0,
                                y: 0.0,
                                pressure: None,
                            },
                            PersistedPointV1 {
                                x: 40.0,
                                y: 0.0,
                                pressure: None,
                            },
                        ],
                    },
                    PersistedFragmentV2 {
                        id: "f2".into(),
                        style: Some(STYLE_LEGACY_FIXED_V1.to_string()),
                        points: vec![
                            PersistedPointV1 {
                                x: 60.0,
                                y: 0.0,
                                pressure: None,
                            },
                            PersistedPointV1 {
                                x: 100.0,
                                y: 0.0,
                                pressure: None,
                            },
                        ],
                    },
                ],
            }],
        )
        .unwrap();
        assert_eq!(
            model.strokes[0].brush.as_deref(),
            Some(STYLE_LEGACY_FIXED_V1)
        );
        assert_eq!(
            model.strokes[1].brush.as_deref(),
            Some(STYLE_LEGACY_FIXED_V1)
        );

        // Second-generation split of f2.
        apply_replace_strokes(
            &mut model,
            vec![PersistedReplacementV2 {
                target_stroke_id: "f2".into(),
                fragments: vec![
                    PersistedFragmentV2 {
                        id: "f2a".into(),
                        style: Some(STYLE_LEGACY_FIXED_V1.to_string()),
                        points: vec![PersistedPointV1 {
                            x: 60.0,
                            y: 0.0,
                            pressure: None,
                        }],
                    },
                    PersistedFragmentV2 {
                        id: "f2b".into(),
                        style: Some(STYLE_LEGACY_FIXED_V1.to_string()),
                        points: vec![PersistedPointV1 {
                            x: 100.0,
                            y: 0.0,
                            pressure: None,
                        }],
                    },
                ],
            }],
        )
        .unwrap();
        for s in &model.strokes {
            if s.id.as_str().starts_with("f2") {
                assert_eq!(s.brush.as_deref(), Some(STYLE_LEGACY_FIXED_V1));
            }
        }
        assert_eq!(
            model.root_of.get("f2a").map(String::as_str),
            Some("s-legacy")
        );
    }

    #[test]
    fn v4_to_v5_refuses_bad_edit_journal_without_rewrite() {
        let root = tmp_dir();
        let bg = vec![0x44u8; 20 * 5];
        let (_s, _) = create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile00()).unwrap();
        let dir = page_dir(&root, "page-1");
        forge_valid_v4(&dir);
        let meta_before = fs::read(meta_path(&dir)).unwrap();
        let mut header: PersistedPageHeader = serde_json::from_slice(&meta_before).unwrap();
        header.edit_journal_version = 99;
        write_meta(&dir, &header).unwrap();
        let poisoned = fs::read(meta_path(&dir)).unwrap();
        let err = migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap_err();
        assert!(err.contains("edit_journal_version"), "{err}");
        assert_eq!(fs::read(meta_path(&dir)).unwrap(), poisoned);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn v4_to_v5_refuses_invalid_render_without_rewrite() {
        let root = tmp_dir();
        let bg = vec![0x55u8; 20 * 5];
        let (_s, _) = create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile00()).unwrap();
        let dir = page_dir(&root, "page-1");
        forge_valid_v4(&dir);
        let mut header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(meta_path(&dir)).unwrap()).unwrap();
        // Downgrade render to fixed-only v1 (not pressure-capable).
        let v1 = RenderProfile::sealed_v1(0x00)
            .unwrap()
            .to_persisted_v1()
            .unwrap();
        header.render = serde_json::to_value(&v1).unwrap();
        write_meta(&dir, &header).unwrap();
        let poisoned = fs::read(meta_path(&dir)).unwrap();
        let err = migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap_err();
        assert!(
            err.contains("pressure") || err.contains("profile") || err.contains("rasterizer"),
            "{err}"
        );
        assert_eq!(fs::read(meta_path(&dir)).unwrap(), poisoned);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn multi_page_v4_to_v5_byte_identical_journals() {
        let root = tmp_dir();
        let bg = vec![0x66u8; 30 * 6];
        for id in ["page-1", "page-2", "page-3"] {
            let (mut store, _) = create_new_page(&root, id, 30, 6, 30, &bg, &profile00()).unwrap();
            store
                .append_committed_stroke(&stroke_with(
                    &format!("s-{id}"),
                    &[(1.0, 1.0), (2.0, 2.0)],
                ))
                .unwrap();
            drop(store);
            forge_valid_v4(&page_dir(&root, id));
        }
        let snapshots: Vec<_> = ["page-1", "page-2", "page-3"]
            .iter()
            .map(|id| {
                let dir = page_dir(&root, id);
                (
                    fs::read(bg_path(&dir)).unwrap(),
                    fs::read(journal_path(&dir)).unwrap(),
                    fs::read(edits_path(&dir)).unwrap(),
                )
            })
            .collect();
        let ids = vec!["page-1".into(), "page-2".into(), "page-3".into()];
        migrate_all_pages(&root, &ids).unwrap();
        for (i, id) in ["page-1", "page-2", "page-3"].iter().enumerate() {
            let dir = page_dir(&root, id);
            let h: PersistedPageHeader =
                serde_json::from_str(&fs::read_to_string(meta_path(&dir)).unwrap()).unwrap();
            assert_eq!(h.v, FORMAT_VERSION);
            assert_eq!(h.edit_journal_version, EDIT_JOURNAL_VERSION);
            assert_eq!(fs::read(bg_path(&dir)).unwrap(), snapshots[i].0);
            assert_eq!(fs::read(journal_path(&dir)).unwrap(), snapshots[i].1);
            assert_eq!(fs::read(edits_path(&dir)).unwrap(), snapshots[i].2);
        }
        // Idempotent.
        migrate_all_pages(&root, &ids).unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn multi_page_migration_refuses_corrupt_and_retries() {
        let root = tmp_dir();
        let bg = vec![0x77u8; 30 * 6];
        for id in ["page-1", "page-2", "page-3"] {
            let (_s, _) = create_new_page(&root, id, 30, 6, 30, &bg, &profile00()).unwrap();
            forge_valid_v4(&page_dir(&root, id));
        }
        // Corrupt page-2 edit_journal_version.
        let dir2 = page_dir(&root, "page-2");
        let mut header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(meta_path(&dir2)).unwrap()).unwrap();
        header.edit_journal_version = 99;
        write_meta(&dir2, &header).unwrap();
        let meta2_poisoned = fs::read(meta_path(&dir2)).unwrap();

        let ids = vec!["page-1".into(), "page-2".into(), "page-3".into()];
        let err = migrate_all_pages(&root, &ids).unwrap_err();
        assert!(err.contains("page-2"), "{err}");
        // page-1 may already be v5 (per-page atomic).
        let h1: PersistedPageHeader = serde_json::from_str(
            &fs::read_to_string(meta_path(&page_dir(&root, "page-1"))).unwrap(),
        )
        .unwrap();
        assert_eq!(h1.v, FORMAT_VERSION);
        // page-2 unchanged.
        assert_eq!(fs::read(meta_path(&dir2)).unwrap(), meta2_poisoned);
        // Retry still refuses.
        let err2 = migrate_all_pages(&root, &ids).unwrap_err();
        assert!(err2.contains("page-2"), "{err2}");
        assert_eq!(fs::read(meta_path(&dir2)).unwrap(), meta2_poisoned);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_live_replace_leaves_edits_jsonl_unchanged() {
        use crate::orch::{FakeTransport, StrokeController};
        use kindle_platform::ClockDomain;

        let root = tmp_dir();
        let bg = vec![0x88u8; 40 * 8];
        let (mut store, _) =
            create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile00()).unwrap();
        store
            .append_committed_stroke(&stroke_with("s-a", &[(0.0, 0.0), (10.0, 0.0)]))
            .unwrap();
        drop(store);
        let edits_before = fs::read(edits_path(&page_dir(&root, "page-1"))).unwrap();

        let (store, loaded) = load_existing_page(&root, "page-1").unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session
            .load_committed_strokes_with_roots(loaded.strokes, loaded.root_of)
            .unwrap();

        // Unknown target — must not append.
        let bad = vec![PersistedReplacementV2 {
            target_stroke_id: "missing".into(),
            fragments: vec![],
        }];
        assert!(ctrl.durable_replace_strokes(&bad).is_err());
        assert_eq!(ctrl.session.committed_count(), 1);
        drop(ctrl);

        assert_eq!(
            fs::read(edits_path(&page_dir(&root, "page-1"))).unwrap(),
            edits_before
        );
        // Page still opens normally.
        let (_s, loaded2) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(loaded2.strokes.len(), 1);
        assert_eq!(loaded2.strokes[0].id.as_str(), "s-a");
        let _ = fs::remove_dir_all(root);
    }
}
