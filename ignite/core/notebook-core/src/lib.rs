//! Platform-independent notebook concepts for the custom Scribe app.
//!
//! Conceptual alignment with Python `scribe.model` for **ink** (`Stroke` / `Point` / page
//! geometry): same domain ideas, no NBK / Ion / actions.log fields. Durable text and
//! cross-content addressing are not in this crate.
//!
//! Geometry in this crate is always **canonical page space**, never framebuffer pixels.
//! See [`coords`] for the device -> page -> display pipeline.
//!
//! Persistence for the Kindle app lives in `scribe-app` (journals + `texts.jsonl`).
//! Cross-replica mutations are `SyncOperationV1` in `sync-protocol`, not [`NotebookOp`].
//! This crate is stroke construction, canonical geometry, transforms, clip/hit primitives,
//! and a session-local [`NotebookOp`] enum used by the ink probe and `PageSession`.

#![forbid(unsafe_code)]

pub mod clip;
pub mod coords;
pub mod hit;
pub mod scribe;

mod ids;
mod ops;
mod point;
mod stroke;
mod transform;

pub use clip::clip_stroke_against_eraser_sweep;
pub use coords::{DevicePoint, DisplayPoint, PageGeometry, PagePoint};
pub use hit::{
    hit_test_strokes_crossing_sweep, hit_test_topmost_stroke, point_segment_distance_sq,
    segment_segment_distance_sq, stroke_hit_test, stroke_intersects_eraser_sweep,
    ERASER_HIT_RADIUS_CANONICAL,
};
#[cfg(feature = "mint")]
pub use ids::new_stroke_id;
pub use ids::StrokeId;
pub use ops::NotebookOp;
pub use point::Point;
pub use scribe::{
    page_geometry as scribe_page_geometry, portrait_wacom_calibration, ScribeDisplayTransform,
    CANONICAL_HEIGHT, CANONICAL_WIDTH, PANEL_HEIGHT_PX, PANEL_TO_CANONICAL_SCALE, PANEL_WIDTH_PX,
};
#[cfg(feature = "mint")]
pub use stroke::{BeginOutcome, StrokeBuilder};
pub use stroke::{Stroke, StrokePhase};
pub use transform::{DeviceCalibration, Orientation, ViewportTransform};
