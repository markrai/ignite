// Sealed Kindle brush semantics, ported exactly (Gate F6 pressure curve, `scribe-raster-*`):
//
//   pressure (f32) → q = round(clamp(p)·4095) → radius band 1..4 (or the missing fallback)
//   canonical point → reference-panel pixel = floor(x·scaleX) clamped (f32 arithmetic)
//   pressure segment → per-Bresenham-step q lerp (integer, truncating) → per-step radius
//
// Radii are in reference-panel pixels (square brush of side 2r+1). The browser draws logical
// width (2r+1) panel pixels = (2r+1)/scale canonical units; antialiasing differs from E Ink,
// the logical widths do not. Pinned by the Rust-generated `tests/fixtures/brush-v1.json`.

import { PRESSURE_Q_MAX, REFERENCE_RASTER } from "../model/generated/protocolConstants";
import type { PressureCurveRef } from "../model/types";

const f32 = Math.fround;

/** `quantize_pressure`: clamp/round semantic pressure into `0..=4095` (f32 math). */
export function quantizePressure(p: number): number {
  if (!Number.isFinite(p)) return 0;
  const c = f32(Math.min(Math.max(f32(p), 0), 1));
  const scaled = f32(c * PRESSURE_Q_MAX);
  // Rust `f32::round` rounds half away from zero; `scaled` ≥ 0 so `Math.round` agrees.
  return Math.min(Math.round(scaled), PRESSURE_Q_MAX);
}

/** `PressureCurveV1::radius_for_q` (bands are 1..4 in the sealed curve). */
export function radiusForQ(curve: PressureCurveRef, q: number): number {
  if (q <= curve.q_r1_max) return 1;
  if (q <= curve.q_r2_max) return 2;
  if (q <= curve.q_r3_max) return 3;
  return 4;
}

/** `PressureCurveV1::radius_for_pressure`. */
export function radiusForPressure(curve: PressureCurveRef, p: number | undefined): number {
  return p !== undefined && Number.isFinite(p) ? radiusForQ(curve, quantizePressure(p)) : curve.missing_fallback_radius;
}

/** `interpolate_q`: integer lerp with truncating division. */
export function interpolateQ(q0: number, q1: number, step: number, steps: number): number {
  if (steps === 0) return q1;
  const v = q0 + Math.trunc(((q1 - q0) * step) / steps);
  return Math.min(Math.max(v, 0), PRESSURE_Q_MAX);
}

/** `ScribeDisplayTransform::to_panel_pixel` for `scribe1-portrait-identity-v1`. */
export function toPanelPixel(x: number, y: number): [number, number] {
  const dx = f32(f32(x) * f32(REFERENCE_RASTER.scaleX));
  const dy = f32(f32(y) * f32(REFERENCE_RASTER.scaleY));
  const px = Math.min(Math.max(Math.floor(dx), 0), REFERENCE_RASTER.width - 1);
  const py = Math.min(Math.max(Math.floor(dy), 0), REFERENCE_RASTER.height - 1);
  return [px, py];
}

/** Iterations of the Kindle Bresenham loop between two panel pixels (0 = single stamp). */
export function bresenhamSteps(x0: number, y0: number, x1: number, y1: number): number {
  return Math.max(Math.abs(x1 - x0), Math.abs(y1 - y0));
}

/** Per-step radii of one pressure segment, exactly as `raster_pressure_segment_panel`. */
export function segmentRadii(
  curve: PressureCurveRef,
  from: [number, number],
  fromP: number | undefined,
  to: [number, number],
  toP: number | undefined,
): number[] {
  const steps = bresenhamSteps(from[0], from[1], to[0], to[1]);
  const finite = (p: number | undefined) => p !== undefined && Number.isFinite(p);
  const out = new Array<number>(steps + 1);
  if (finite(fromP) && finite(toP)) {
    const q0 = quantizePressure(fromP!);
    const q1 = quantizePressure(toP!);
    for (let s = 0; s <= steps; s++) out[s] = radiusForQ(curve, interpolateQ(q0, q1, s, steps));
  } else {
    out.fill(curve.missing_fallback_radius);
  }
  return out;
}

/** Logical brush width in canonical units for a radius-`r` square brush. */
export function canonicalWidthForRadius(r: number): number {
  return (2 * r + 1) / REFERENCE_RASTER.scaleX;
}
