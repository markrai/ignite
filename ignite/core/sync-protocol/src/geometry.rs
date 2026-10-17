//! Canonical sync geometry: page-space coordinates, never pixels.
//!
//! Audit (S1): `notebook_core::Point` holds `x, y: f32` in canonical page units
//! (15624 × 20832), `pressure: Option<f32>` normalized to `0.0..=1.0`, and a capture-local
//! `t_us` that is never persisted. Durable `PersistedPointV1` stores exactly `x, y` (f32) and
//! optional `pressure` (f32). The wire therefore uses the same f32 values: serde_json writes the
//! shortest decimal that round-trips an f32, so existing durable geometry crosses the protocol
//! bit-exactly (proven by round-trip tests). Framebuffer (1860 × 2480), browser canvas,
//! touchscreen and Wacom device units are never part of the protocol.

use serde::{Deserialize, Serialize};

use crate::error::{refuse, Result};

/// Canonical page width in page units (must equal `notebook_core::CANONICAL_WIDTH`).
pub const CANONICAL_PAGE_WIDTH: f32 = 15_624.0;
/// Canonical page height in page units (must equal `notebook_core::CANONICAL_HEIGHT`).
pub const CANONICAL_PAGE_HEIGHT: f32 = 20_832.0;

/// Self-description of the coordinate space every snapshot/operation uses.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSpace {
    pub width: f32,
    pub height: f32,
}

impl PageSpace {
    pub const CANONICAL: PageSpace = PageSpace {
        width: CANONICAL_PAGE_WIDTH,
        height: CANONICAL_PAGE_HEIGHT,
    };

    pub fn validate(&self) -> Result<()> {
        if *self != Self::CANONICAL {
            refuse!(
                "unsupported page space {}x{} (need {CANONICAL_PAGE_WIDTH}x{CANONICAL_PAGE_HEIGHT})",
                self.width,
                self.height
            );
        }
        Ok(())
    }
}

/// One ink sample in canonical page space. `pressure` is absent when the device reported none.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPoint {
    pub x: f32,
    pub y: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<f32>,
}

impl SyncPoint {
    pub fn new(x: f32, y: f32, pressure: Option<f32>) -> Self {
        Self { x, y, pressure }
    }

    /// Reject (never clamp): NaN, ±Inf, coordinates outside the page, pressure outside 0..=1.
    pub fn validate(&self) -> Result<()> {
        if !self.x.is_finite() || !self.y.is_finite() {
            refuse!("non-finite point x={:?} y={:?}", self.x, self.y);
        }
        if !(0.0..=CANONICAL_PAGE_WIDTH).contains(&self.x)
            || !(0.0..=CANONICAL_PAGE_HEIGHT).contains(&self.y)
        {
            refuse!(
                "point outside canonical page ({CANONICAL_PAGE_WIDTH}x{CANONICAL_PAGE_HEIGHT}): x={} y={}",
                self.x,
                self.y
            );
        }
        if let Some(p) = self.pressure {
            if !p.is_finite() || !(0.0..=1.0).contains(&p) {
                refuse!("invalid pressure {p:?} (need finite 0.0..=1.0)");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_geometry_round_trips_bit_exactly_through_json() {
        // Deterministic pseudo-random sweep over the canonical range (xorshift, no RNG dep).
        let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..200_000 {
            let r = next();
            let x = (r as u32 as f32 / u32::MAX as f32) * CANONICAL_PAGE_WIDTH;
            let y = ((r >> 32) as u32 as f32 / u32::MAX as f32) * CANONICAL_PAGE_HEIGHT;
            let p = (next() as u32 as f32) / u32::MAX as f32;
            let pt = SyncPoint::new(x, y, Some(p));
            pt.validate().unwrap();
            let json = serde_json::to_string(&pt).unwrap();
            let back: SyncPoint = serde_json::from_str(&json).unwrap();
            assert_eq!(back.x.to_bits(), x.to_bits(), "{json}");
            assert_eq!(back.y.to_bits(), y.to_bits(), "{json}");
            assert_eq!(back.pressure.unwrap().to_bits(), p.to_bits(), "{json}");
        }
    }

    #[test]
    fn invalid_geometry_is_refused() {
        for pt in [
            SyncPoint::new(f32::NAN, 1.0, None),
            SyncPoint::new(1.0, f32::INFINITY, None),
            SyncPoint::new(-0.5, 1.0, None),
            SyncPoint::new(CANONICAL_PAGE_WIDTH + 1.0, 1.0, None),
            SyncPoint::new(1.0, CANONICAL_PAGE_HEIGHT + 1.0, None),
            SyncPoint::new(1.0, 1.0, Some(1.01)),
            SyncPoint::new(1.0, 1.0, Some(-0.1)),
            SyncPoint::new(1.0, 1.0, Some(f32::NAN)),
        ] {
            assert!(pt.validate().is_err(), "{pt:?}");
        }
        SyncPoint::new(0.0, CANONICAL_PAGE_HEIGHT, Some(0.0))
            .validate()
            .unwrap();
        assert!(serde_json::from_str::<SyncPoint>(r#"{"x":1,"y":2,"t_us":5}"#).is_err());
        assert_eq!(
            serde_json::to_string(&SyncPoint::new(1.5, 2.0, None)).unwrap(),
            r#"{"x":1.5,"y":2.0}"#
        );
    }
}
