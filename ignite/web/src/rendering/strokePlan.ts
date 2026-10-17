// Stroke → logical drawing commands in CANONICAL page units (no canvas, no viewport).
//
// Mirrors the Kindle rasterizer's decisions (`scribe-app/src/render.rs::raster_stroke_into`):
// * legacy-fixed-v1: every sample joined at the profile's fixed radius;
// * pressure-width-v1: first sample stamped at its own radius, then every segment walks the
//   Bresenham steps of its reference-panel projection with the per-step interpolated radius.
// Runs of equal radius become one round-capped polyline; where the radius changes, the run
// ends at that step's position and the next run starts there, so round caps overlap and no
// gap can appear between widths.

import {
  RASTERIZER_ID_V1,
  RASTERIZER_ID_V2_PRESSURE,
  RENDER_PROFILE_VERSION_V1,
  RENDER_PROFILE_VERSION_V2,
  TRANSFORM_ID_V1,
} from "../model/generated/protocolConstants";
import type { PressureCurveRef, RenderProfileRef, SyncStroke } from "../model/types";
import { canonicalWidthForRadius, radiusForPressure, segmentRadii, toPanelPixel } from "./brush";

export interface Pt {
  x: number;
  y: number;
}

/** One round-capped, round-joined polyline of constant logical width (canonical units). */
export interface DrawRun {
  radius: number;
  width: number;
  points: Pt[];
}

export class UnsupportedRenderProfile extends Error {
  constructor(message: string) {
    super(message);
    this.name = "UnsupportedRenderProfile";
  }
}

/** The sealed profiles this viewer knows how to draw; anything else is a visible page error. */
export function checkRenderProfile(r: RenderProfileRef): void {
  if (r.transform_id !== TRANSFORM_ID_V1) {
    throw new UnsupportedRenderProfile(`unsupported transform ${r.transform_id}`);
  }
  const v1 = r.profile_version === RENDER_PROFILE_VERSION_V1 && r.rasterizer_id === RASTERIZER_ID_V1 && !r.pressure;
  const v2 =
    r.profile_version === RENDER_PROFILE_VERSION_V2 &&
    r.rasterizer_id === RASTERIZER_ID_V2_PRESSURE &&
    !!r.pressure &&
    r.pressure.min_radius === 1 &&
    r.pressure.max_radius === 4;
  if (!v1 && !v2) {
    throw new UnsupportedRenderProfile(
      `unsupported render profile v${r.profile_version} ${r.rasterizer_id}`,
    );
  }
}

class RunBuilder {
  runs: DrawRun[] = [];
  private cur: DrawRun | null = null;

  point(p: Pt, radius: number): void {
    if (this.cur && this.cur.radius === radius) {
      const last = this.cur.points[this.cur.points.length - 1]!;
      if (last.x !== p.x || last.y !== p.y) this.cur.points.push(p);
      return;
    }
    // New width starts where the previous run ended (overlapping round caps, no gap).
    const start = this.cur ? this.cur.points[this.cur.points.length - 1]! : p;
    this.cur = { radius, width: canonicalWidthForRadius(radius), points: [start] };
    if (start.x !== p.x || start.y !== p.y) this.cur.points.push(p);
    this.runs.push(this.cur);
  }
}

function lerp(a: Pt, b: Pt, t: number): Pt {
  return { x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t };
}

export function planStroke(stroke: SyncStroke, render: RenderProfileRef): DrawRun[] {
  const b = new RunBuilder();
  const pts = stroke.points;
  if (pts.length === 0) return [];
  if (stroke.style === "legacy-fixed-v1") {
    for (const p of pts) b.point({ x: p.x, y: p.y }, render.legacy_fixed_radius);
    return b.runs;
  }
  const curve: PressureCurveRef | undefined = render.pressure;
  if (!curve) throw new UnsupportedRenderProfile("pressure stroke without a pressure curve");
  const first = pts[0]!;
  b.point({ x: first.x, y: first.y }, radiusForPressure(curve, first.pressure));
  for (let i = 1; i < pts.length; i++) {
    const a = pts[i - 1]!;
    const c = pts[i]!;
    const radii = segmentRadii(curve, toPanelPixel(a.x, a.y), a.pressure, toPanelPixel(c.x, c.y), c.pressure);
    const steps = radii.length - 1;
    // Points only where the radius changes (at that exact step) and at segment ends.
    let r = radii[0]!;
    b.point({ x: a.x, y: a.y }, r);
    for (let s = 1; s <= steps; s++) {
      const next = radii[s]!;
      if (next !== r) {
        const at = lerp(a, c, s / steps);
        b.point(at, r);
        b.point(at, next);
        r = next;
      }
    }
    b.point({ x: c.x, y: c.y }, r);
  }
  return b.runs;
}
