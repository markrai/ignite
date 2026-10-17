//! Shared content value types used by both snapshots and operations.

use serde::{Deserialize, Serialize};

use crate::error::{refuse, Result};
use crate::geometry::{SyncPoint, CANONICAL_PAGE_HEIGHT, CANONICAL_PAGE_WIDTH};
use crate::ids::{LinkId, NotebookId, PageId, StrokeId, TextId};

/// Longest accepted notebook title (matches the Kindle title editor).
pub const MAX_TITLE_CHARS: usize = 96;
/// Upper bound on points in one stroke (defensive; real strokes are far smaller).
pub const MAX_STROKE_POINTS: usize = 100_000;
/// Longest accepted text-object content, counted as Unicode scalar values.
pub const MAX_TEXT_CHARS: usize = 8192;
/// Smallest legal `font_size` in canonical page units.
pub const MIN_TEXT_FONT_SIZE: f32 = 1.0;
/// Largest legal `font_size` in canonical page units (defensive).
pub const MAX_TEXT_FONT_SIZE: f32 = CANONICAL_PAGE_HEIGHT;
/// Wire token for the v1 default face (see `docs/SYNC_ARCHITECTURE.md`).
pub const DEFAULT_SANS_V1_FACE: &str = "default-sans-v1";

/// Notebook title rules shared by every replica: non-empty after trimming, at most
/// [`MAX_TITLE_CHARS`] characters, no control characters.
pub fn validate_title(title: &str) -> Result<()> {
    if title.trim().is_empty() {
        refuse!("invalid title: empty after trimming");
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        refuse!("invalid title: longer than {MAX_TITLE_CHARS} characters");
    }
    if title.chars().any(char::is_control) {
        refuse!("invalid title: control characters are not allowed");
    }
    Ok(())
}

/// Brush semantics of a stroke (the local "missing style" is legacy-fixed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrokeStyle {
    /// Fixed-radius ink; pressure (if present) does not affect width.
    #[serde(rename = "legacy-fixed-v1")]
    LegacyFixedV1,
    /// Width from the page render profile's pressure curve.
    #[serde(rename = "pressure-width-v1")]
    PressureWidthV1,
}

/// One visible stroke or erase fragment.
///
/// `stroke_id` identifies this exact visible geometry. `root_id` is the authored lineage root:
/// equal to `stroke_id` for an unerased stroke, and the original stroke's ID for every fragment
/// a partial erase produced (possibly a root that is no longer visible itself). Lineage is
/// recorded, never inferred from geometry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncStroke {
    pub stroke_id: StrokeId,
    pub root_id: StrokeId,
    pub style: StrokeStyle,
    pub points: Vec<SyncPoint>,
}

impl SyncStroke {
    pub fn validate(&self) -> Result<()> {
        if self.points.is_empty() {
            refuse!("stroke {} has no points", self.stroke_id);
        }
        if self.points.len() > MAX_STROKE_POINTS {
            refuse!(
                "stroke {} exceeds {MAX_STROKE_POINTS} points",
                self.stroke_id
            );
        }
        for p in &self.points {
            p.validate().map_err(|e| {
                crate::ProtocolError::new(format!("stroke {}: {e}", self.stroke_id))
            })?;
        }
        Ok(())
    }
}

/// Abstract face token. The wire name is not a CSS/system font; it names the
/// `default-sans-v1` typography contract (bundled font + metrics + wrap).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextFace {
    #[serde(rename = "default-sans-v1")]
    DefaultSansV1,
}

/// One positioned page text object. Height is not stored: it is derived from
/// `content`, `font_size`, `width`, and the face's line metrics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncText {
    pub text_id: TextId,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub font_size: f32,
    pub face: TextFace,
    pub content: String,
}

impl SyncText {
    pub fn validate(&self) -> Result<()> {
        if !self.x.is_finite() || !self.y.is_finite() {
            refuse!(
                "text {} has non-finite origin x={:?} y={:?}",
                self.text_id,
                self.x,
                self.y
            );
        }
        if !(0.0..CANONICAL_PAGE_WIDTH).contains(&self.x)
            || !(0.0..CANONICAL_PAGE_HEIGHT).contains(&self.y)
        {
            refuse!(
                "text {} origin outside canonical page: x={} y={}",
                self.text_id,
                self.x,
                self.y
            );
        }
        if !self.width.is_finite() || self.width <= 0.0 {
            refuse!("text {} has invalid width {:?}", self.text_id, self.width);
        }
        let max_width = CANONICAL_PAGE_WIDTH - self.x;
        if self.width > max_width {
            refuse!(
                "text {} width {} exceeds remaining page width {max_width}",
                self.text_id,
                self.width
            );
        }
        if !self.font_size.is_finite()
            || self.font_size < MIN_TEXT_FONT_SIZE
            || self.font_size > MAX_TEXT_FONT_SIZE
        {
            refuse!(
                "text {} has invalid font_size {:?}",
                self.text_id,
                self.font_size
            );
        }
        validate_text_content(&self.content)
            .map_err(|e| crate::ProtocolError::new(format!("text {}: {e}", self.text_id)))
    }
}

/// Normalize authored text before it is stored: `\r\n` / `\r` → `\n`. Nothing else is
/// rewritten (no trim). Callers persist the result.
pub fn normalize_text_content(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// One explicit spatial link from a point on a source page to a target page.
///
/// Identity is [`LinkId`] plus global notebook/page IDs. Notebook titles and page
/// numbers are display metadata only and must not be stored here. Same-page
/// self-links (`source` notebook+page == `target` notebook+page) are forbidden.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPageLink {
    pub link_id: LinkId,
    pub source_notebook_id: NotebookId,
    pub source_page_id: PageId,
    pub source_x: f32,
    pub source_y: f32,
    pub target_notebook_id: NotebookId,
    pub target_page_id: PageId,
}

impl SyncPageLink {
    pub fn validate(&self) -> Result<()> {
        if !self.source_x.is_finite() || !self.source_y.is_finite() {
            refuse!(
                "link {} has non-finite origin x={:?} y={:?}",
                self.link_id,
                self.source_x,
                self.source_y
            );
        }
        if !(0.0..CANONICAL_PAGE_WIDTH).contains(&self.source_x)
            || !(0.0..CANONICAL_PAGE_HEIGHT).contains(&self.source_y)
        {
            refuse!(
                "link {} origin outside canonical page: x={} y={}",
                self.link_id,
                self.source_x,
                self.source_y
            );
        }
        if self.source_notebook_id == self.target_notebook_id
            && self.source_page_id == self.target_page_id
        {
            refuse!(
                "link {} cannot target its source page {}/{}",
                self.link_id,
                self.source_notebook_id,
                self.source_page_id
            );
        }
        Ok(())
    }
}

/// Content rules for Add/Update: non-empty, ≤ [`MAX_TEXT_CHARS`] Unicode scalars, no
/// control characters other than `\n`. The string must already be line-ending-normalized.
pub fn validate_text_content(content: &str) -> Result<()> {
    if content.is_empty() {
        refuse!("invalid text content: empty");
    }
    if content.chars().count() > MAX_TEXT_CHARS {
        refuse!("invalid text content: longer than {MAX_TEXT_CHARS} characters");
    }
    if content.contains('\r') {
        refuse!("invalid text content: CR must be normalized to LF before persist");
    }
    if content.chars().any(|c| c.is_control() && c != '\n') {
        refuse!("invalid text content: control characters other than newline are not allowed");
    }
    Ok(())
}

/// Lowercase hex SHA-256 digest (64 chars).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Hex(String);

impl Sha256Hex {
    pub fn parse(s: &str) -> Result<Self> {
        if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            refuse!("invalid sha256 {s:?}: need 64 lowercase hex characters");
        }
        Ok(Self(s.to_string()))
    }

    pub fn from_digest(d: &[u8; 32]) -> Self {
        let mut s = String::with_capacity(64);
        for b in d {
            s.push_str(&format!("{b:02x}"));
        }
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Sha256Hex {
    type Error = crate::ProtocolError;

    fn try_from(s: String) -> Result<Self> {
        Self::parse(&s)
    }
}

impl From<Sha256Hex> for String {
    fn from(h: Sha256Hex) -> String {
        h.0
    }
}

/// Pixel encoding of a durable background blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundEncoding {
    /// One byte per pixel, 0x00 black … 0xff white, rows `stride` bytes apart.
    Gray8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundFormat {
    pub encoding: BackgroundEncoding,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Page background by reference. `Blank` is uniform white paper (no bytes needed anywhere);
/// `Blob` names content-addressed durable pixels. S1 defines references only: nothing is
/// uploaded or stored remotely, and the live framebuffer never contributes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackgroundRef {
    Blank,
    Blob {
        sha256: Sha256Hex,
        byte_len: u64,
        format: BackgroundFormat,
    },
}

impl BackgroundRef {
    pub fn validate(&self) -> Result<()> {
        if let BackgroundRef::Blob {
            byte_len, format, ..
        } = self
        {
            if format.width == 0 || format.height == 0 || format.stride < format.width {
                refuse!("invalid background format {format:?}");
            }
            let want = u64::from(format.stride) * u64::from(format.height);
            if *byte_len != want {
                refuse!("background byte_len {byte_len} != stride*height {want}");
            }
        }
        Ok(())
    }
}

/// Pressure-width parameters of a sealed render profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PressureCurveRef {
    pub q_r1_max: u16,
    pub q_r2_max: u16,
    pub q_r3_max: u16,
    pub min_radius: u32,
    pub max_radius: u32,
    pub missing_fallback_radius: u32,
}

/// The page's sealed stroke appearance (ink pigment + width rules). Identifies how strokes
/// look; each platform maps it to its own rasterizer. Radii are in the reference Kindle
/// raster's units and identify the sealed profile — they are not coordinates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderProfileRef {
    pub profile_version: u32,
    pub ink_byte: u8,
    pub legacy_fixed_radius: u32,
    pub transform_id: String,
    pub rasterizer_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<PressureCurveRef>,
}

impl RenderProfileRef {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.ink_byte, 0x00 | 0xff) {
            refuse!(
                "invalid ink_byte {:#04x} (need 0x00 or 0xff)",
                self.ink_byte
            );
        }
        for (name, v) in [
            ("transform_id", &self.transform_id),
            ("rasterizer_id", &self.rasterizer_id),
        ] {
            if v.is_empty() || v.len() > 64 || !v.bytes().all(|b| b.is_ascii_graphic()) {
                refuse!("invalid render {name} {v:?}");
            }
        }
        if let Some(p) = self.pressure {
            if !(p.q_r1_max <= p.q_r2_max && p.q_r2_max <= p.q_r3_max)
                || p.min_radius > p.max_radius
            {
                refuse!("invalid pressure curve {p:?}");
            }
        }
        Ok(())
    }
}
