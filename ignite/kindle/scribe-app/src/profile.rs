//! Gate F1.1 / F6 sealed render profile for deterministic reconstruction.
//!
//! `scribe-raster-v1` (page format ≤3): fixed brush radius 2 only.
//! `scribe-raster-v2-pressure` (page format 4): legacy strokes fixed r=2; pressure-width
//! strokes use sealed [`PressureCurveV1`].

use kindle_platform::FbGeometry;
use notebook_core::ScribeDisplayTransform;
use serde::{Deserialize, Serialize};

use crate::pressure_curve::{PressureCurveV1, PRESSURE_CURVE_ID_V1};

/// Nested render-profile schema: v1 = fixed-width only; v2 = pressure-capable.
pub const RENDER_PROFILE_VERSION_V1: u32 = 1;
pub const RENDER_PROFILE_VERSION: u32 = 2;

pub const RASTERIZER_ID_V1: &str = "scribe-raster-v1";
pub const RASTERIZER_ID_V2_PRESSURE: &str = "scribe-raster-v2-pressure";

pub const TRANSFORM_ID_V1: &str = "scribe1-portrait-identity-v1";

/// Legacy fixed radius (also missing-pressure fallback).
pub const LEGACY_FIXED_RADIUS: u32 = 2;

/// Persisted page render profile (format v2–v3 nested under page.meta).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedRenderProfileV1 {
    pub profile_version: u32,
    pub ink_byte: u8,
    pub brush_radius: u32,
    pub transform: String,
    pub rasterizer: String,
}

/// Persisted page render profile for format v4 (pressure-capable).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedRenderProfileV2 {
    pub profile_version: u32,
    pub ink_byte: u8,
    /// Fixed radius for legacy-fixed strokes.
    pub legacy_fixed_radius: u32,
    pub pressure_min_radius: u32,
    pub pressure_max_radius: u32,
    /// Sealed radius when a pressure-width sample has `pressure = None`.
    pub pressure_missing_fallback_radius: u32,
    pub pressure_curve: String,
    pub pressure_q_r1_max: u16,
    pub pressure_q_r2_max: u16,
    pub pressure_q_r3_max: u16,
    pub transform: String,
    pub rasterizer: String,
}

/// Authoritative in-memory render profile for one open page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderProfile {
    pub profile_version: u32,
    pub ink_byte: u8,
    /// Fixed radius for legacy / missing-pressure (always 2 for sealed profiles).
    pub brush_radius: u32,
    pub transform_id: String,
    pub rasterizer_id: String,
    pub pressure: Option<PressureCurveV1>,
}

impl RenderProfile {
    /// Pre-F6 sealed profile (fixed r=2 only). Used for byte-identity fixtures.
    #[allow(dead_code)]
    pub fn sealed_v1(ink_byte: u8) -> Result<Self, String> {
        validate_ink(ink_byte)?;
        Ok(Self {
            profile_version: RENDER_PROFILE_VERSION_V1,
            ink_byte,
            brush_radius: LEGACY_FIXED_RADIUS,
            transform_id: TRANSFORM_ID_V1.into(),
            rasterizer_id: RASTERIZER_ID_V1.into(),
            pressure: None,
        })
    }

    /// F6 sealed profile: legacy fixed r=2 + pressure curve.
    pub fn sealed_v2_pressure(ink_byte: u8) -> Result<Self, String> {
        validate_ink(ink_byte)?;
        let curve = PressureCurveV1::sealed_v1();
        Ok(Self {
            profile_version: RENDER_PROFILE_VERSION,
            ink_byte,
            brush_radius: LEGACY_FIXED_RADIUS,
            transform_id: TRANSFORM_ID_V1.into(),
            rasterizer_id: RASTERIZER_ID_V2_PRESSURE.into(),
            pressure: Some(curve),
        })
    }

    pub fn to_persisted_v1(&self) -> Result<PersistedRenderProfileV1, String> {
        if self.profile_version != RENDER_PROFILE_VERSION_V1 || self.pressure.is_some() {
            return Err("to_persisted_v1 requires sealed v1 profile".into());
        }
        Ok(PersistedRenderProfileV1 {
            profile_version: self.profile_version,
            ink_byte: self.ink_byte,
            brush_radius: self.brush_radius,
            transform: self.transform_id.clone(),
            rasterizer: self.rasterizer_id.clone(),
        })
    }

    pub fn to_persisted_v2(&self) -> Result<PersistedRenderProfileV2, String> {
        let curve = self
            .pressure
            .ok_or_else(|| "to_persisted_v2 requires pressure curve".to_string())?;
        if self.profile_version != RENDER_PROFILE_VERSION {
            return Err("to_persisted_v2 requires sealed v2 profile".into());
        }
        Ok(PersistedRenderProfileV2 {
            profile_version: RENDER_PROFILE_VERSION,
            ink_byte: self.ink_byte,
            legacy_fixed_radius: self.brush_radius,
            pressure_min_radius: curve.min_radius,
            pressure_max_radius: curve.max_radius,
            pressure_missing_fallback_radius: curve.missing_fallback_radius,
            pressure_curve: PRESSURE_CURVE_ID_V1.into(),
            pressure_q_r1_max: curve.q_r1_max,
            pressure_q_r2_max: curve.q_r2_max,
            pressure_q_r3_max: curve.q_r3_max,
            transform: self.transform_id.clone(),
            rasterizer: self.rasterizer_id.clone(),
        })
    }

    pub fn transform(&self) -> Result<ScribeDisplayTransform, String> {
        if self.transform_id == TRANSFORM_ID_V1 {
            Ok(ScribeDisplayTransform::PortraitIdentity)
        } else {
            Err(format!(
                "REFUSE PAGE OPEN: unsupported transform id {}",
                self.transform_id
            ))
        }
    }

    /// Pressure curve for pressure-width strokes. Refuses under a fixed-only (v1) profile.
    pub fn pressure_curve(&self) -> Result<PressureCurveV1, String> {
        self.pressure.ok_or_else(|| {
            "REFUSE RENDER: pressure-width stroke requires pressure-capable render profile".into()
        })
    }
}

fn validate_ink(ink_byte: u8) -> Result<(), String> {
    if ink_byte != 0x00 && ink_byte != 0xff {
        return Err(format!(
            "REFUSE: unsupported ink_byte {ink_byte:#04x} (need 00 or ff)"
        ));
    }
    Ok(())
}

/// Validate a v1 persisted profile (pre-F6 pages / tests).
pub fn validate_persisted_render_profile_v1(
    p: &PersistedRenderProfileV1,
) -> Result<RenderProfile, String> {
    if p.profile_version != RENDER_PROFILE_VERSION_V1 {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported render profile_version {} (need {RENDER_PROFILE_VERSION_V1})",
            p.profile_version
        ));
    }
    if p.rasterizer != RASTERIZER_ID_V1 {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported rasterizer id {} (need {RASTERIZER_ID_V1})",
            p.rasterizer
        ));
    }
    if p.transform != TRANSFORM_ID_V1 {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported transform id {} (need {TRANSFORM_ID_V1})",
            p.transform
        ));
    }
    if p.brush_radius != LEGACY_FIXED_RADIUS {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported brush_radius {} (need {LEGACY_FIXED_RADIUS})",
            p.brush_radius
        ));
    }
    validate_ink(p.ink_byte)?;
    Ok(RenderProfile {
        profile_version: p.profile_version,
        ink_byte: p.ink_byte,
        brush_radius: p.brush_radius,
        transform_id: p.transform.clone(),
        rasterizer_id: p.rasterizer.clone(),
        pressure: None,
    })
}

pub fn validate_persisted_render_profile_v2(
    p: &PersistedRenderProfileV2,
) -> Result<RenderProfile, String> {
    if p.profile_version != RENDER_PROFILE_VERSION {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported render profile_version {} (need {RENDER_PROFILE_VERSION})",
            p.profile_version
        ));
    }
    if p.rasterizer != RASTERIZER_ID_V2_PRESSURE {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported rasterizer id {} (need {RASTERIZER_ID_V2_PRESSURE})",
            p.rasterizer
        ));
    }
    if p.transform != TRANSFORM_ID_V1 {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported transform id {} (need {TRANSFORM_ID_V1})",
            p.transform
        ));
    }
    if p.legacy_fixed_radius != LEGACY_FIXED_RADIUS {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported legacy_fixed_radius {} (need {LEGACY_FIXED_RADIUS})",
            p.legacy_fixed_radius
        ));
    }
    if p.pressure_curve != PRESSURE_CURVE_ID_V1 {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported pressure_curve {}",
            p.pressure_curve
        ));
    }
    let sealed = PressureCurveV1::sealed_v1();
    if p.pressure_min_radius != sealed.min_radius
        || p.pressure_max_radius != sealed.max_radius
        || p.pressure_missing_fallback_radius != sealed.missing_fallback_radius
        || p.pressure_q_r1_max != sealed.q_r1_max
        || p.pressure_q_r2_max != sealed.q_r2_max
        || p.pressure_q_r3_max != sealed.q_r3_max
    {
        return Err(
            "REFUSE PAGE OPEN: pressure curve thresholds do not match sealed F6 curve".into(),
        );
    }
    validate_ink(p.ink_byte)?;
    Ok(RenderProfile {
        profile_version: p.profile_version,
        ink_byte: p.ink_byte,
        brush_radius: p.legacy_fixed_radius,
        transform_id: p.transform.clone(),
        rasterizer_id: p.rasterizer.clone(),
        pressure: Some(PressureCurveV1 {
            q_r1_max: p.pressure_q_r1_max,
            q_r2_max: p.pressure_q_r2_max,
            q_r3_max: p.pressure_q_r3_max,
            min_radius: p.pressure_min_radius,
            max_radius: p.pressure_max_radius,
            missing_fallback_radius: p.pressure_missing_fallback_radius,
        }),
    })
}

pub fn validate_framebuffer_vs_page(
    panel_width: u32,
    panel_height: u32,
    line_length: u32,
    bg_layout_version: u32,
    expected_bg_layout: u32,
    geo: &FbGeometry,
) -> Result<(), String> {
    if bg_layout_version != expected_bg_layout {
        return Err(format!(
            "REFUSE PAGE OPEN: unsupported bg_layout_version {bg_layout_version}"
        ));
    }
    if geo.bits_per_pixel != 8 {
        return Err(format!(
            "REFUSE PAGE OPEN: framebuffer geometry incompatible with persisted page (bits_per_pixel={} need 8)",
            geo.bits_per_pixel
        ));
    }
    if geo.xres != panel_width || geo.yres != panel_height || geo.line_length != line_length {
        return Err(format!(
            "REFUSE PAGE OPEN: framebuffer geometry incompatible with persisted page \
             (fb {}x{} stride={} vs page {}x{} stride={})",
            geo.xres, geo.yres, geo.line_length, panel_width, panel_height, line_length
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_v1_round_trip() {
        let p = RenderProfile::sealed_v1(0x00).unwrap();
        let back = validate_persisted_render_profile_v1(&p.to_persisted_v1().unwrap()).unwrap();
        assert_eq!(back.ink_byte, 0x00);
        assert_eq!(back.brush_radius, LEGACY_FIXED_RADIUS);
        assert_eq!(back.rasterizer_id, RASTERIZER_ID_V1);
        assert!(back.pressure.is_none());
    }

    #[test]
    fn sealed_v2_round_trip() {
        let p = RenderProfile::sealed_v2_pressure(0xff).unwrap();
        let back = validate_persisted_render_profile_v2(&p.to_persisted_v2().unwrap()).unwrap();
        assert_eq!(back.rasterizer_id, RASTERIZER_ID_V2_PRESSURE);
        assert!(back.pressure.is_some());
    }

    #[test]
    fn refuse_bad_rasterizer() {
        let mut p = RenderProfile::sealed_v1(0xff)
            .unwrap()
            .to_persisted_v1()
            .unwrap();
        p.rasterizer = "other".into();
        assert!(validate_persisted_render_profile_v1(&p)
            .unwrap_err()
            .contains("rasterizer"));
    }
}
