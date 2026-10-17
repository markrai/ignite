// S3B stroke capture: browser pointer samples → canonical AddStroke points. Pure; no DOM.
//
// Geometry contract (same as the Kindle, `notebook-core` transform): points are canonical page
// units (15624 × 20832 for PageSpace::CANONICAL). A stroke must START on the page; later samples
// that leave it are clamped to the page edge (the Kindle clamps normalized device coordinates
// to [0, 1] the same way). The protocol refuses anything outside [0, W] × [0, H].
//
// No smoothing, resampling or prediction: every accepted sample is stored as-is (f32), except
// that an exact repeat of the previous stored point is dropped.

import type { StrokeStyle, SyncPoint } from "../model/types";
import type { ScreenTransform, Size } from "../rendering/viewport";

/** Browser-side cap (the protocol allows 100 000; a batch body is capped at 4 MiB). */
export const MAX_STROKE_POINTS = 20_000;

export type InkPointerType = "pen" | "mouse" | "touch";

export interface Sample {
  /** CSS px relative to the page viewport element's top-left. */
  localX: number;
  localY: number;
  /** PointerEvent.pressure (0..1). */
  pressure: number;
}

/** Inverse of `toScreen`: viewport CSS px → canonical page units (unclamped). */
export function toCanonical(t: ScreenTransform, localX: number, localY: number): { x: number; y: number } {
  return { x: (localX - t.offsetX) / t.scale, y: (localY - t.offsetY) / t.scale };
}

export function insidePage(p: { x: number; y: number }, page: Size): boolean {
  return p.x >= 0 && p.y >= 0 && p.x <= page.width && p.y <= page.height;
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

/**
 * One eraser gesture's sweep path (S3B erase slice). Like the Kindle: the Down point, then each
 * sample; no pressure; the reducer sweeps `(p0,p0), (p0,p1), …` with the Kindle radius. Starts
 * must be on the page; later samples are clamped; exact consecutive repeats are dropped (a
 * zero-length sweep repeats the previous one exactly, so dropping it changes nothing).
 */
export class EraseCapture {
  private readonly pts: { x: number; y: number }[] = [];
  private overflow = false;

  private constructor(private readonly page: Size) {}

  static begin(t: ScreenTransform, page: Size, s: Sample): EraseCapture | null {
    const p = toCanonical(t, s.localX, s.localY);
    if (!Number.isFinite(p.x) || !Number.isFinite(p.y) || !insidePage(p, page)) return null;
    const c = new EraseCapture(page);
    c.add(t, s);
    return c;
  }

  get path(): readonly { x: number; y: number }[] {
    return this.pts;
  }

  get tooLong(): boolean {
    return this.overflow;
  }

  /** Returns true if a sample was stored. */
  add(t: ScreenTransform, s: Sample): boolean {
    const raw = toCanonical(t, s.localX, s.localY);
    if (!Number.isFinite(raw.x) || !Number.isFinite(raw.y)) return false;
    const x = Math.fround(clamp(raw.x, 0, this.page.width));
    const y = Math.fround(clamp(raw.y, 0, this.page.height));
    const last = this.pts[this.pts.length - 1];
    if (last && last.x === x && last.y === y) return false;
    if (this.pts.length >= MAX_STROKE_POINTS) {
      this.overflow = true;
      return false;
    }
    this.pts.push({ x, y });
    return true;
  }
}

export interface FinishedStroke {
  style: StrokeStyle;
  points: SyncPoint[];
}

export type FinishResult = { kind: "stroke"; stroke: FinishedStroke } | { kind: "too-long" } | { kind: "empty" };

/**
 * One in-progress stroke. `begin` refuses a start outside the page; `finish` yields exactly one
 * stroke (or a refusal). A cancelled capture is simply dropped by the caller — it authors nothing.
 */
export class StrokeCapture {
  private readonly pts: SyncPoint[] = [];
  private overflow = false;
  /** Pen samples with real (non-zero) pressure seen. */
  private sawPressure = false;

  private constructor(
    readonly pointerType: InkPointerType,
    private readonly page: Size,
    private readonly pressureProfile: boolean,
  ) {}

  /**
   * `pressureProfile`: the target page's render profile has a pressure curve (otherwise every
   * stroke is authored as LegacyFixedV1, which is all such a page can render).
   */
  static begin(pointerType: InkPointerType, t: ScreenTransform, page: Size, s: Sample, pressureProfile = true): StrokeCapture | null {
    const p = toCanonical(t, s.localX, s.localY);
    if (!Number.isFinite(p.x) || !Number.isFinite(p.y) || !insidePage(p, page)) return null;
    const c = new StrokeCapture(pointerType, page, pressureProfile);
    c.add(t, s);
    return c;
  }

  get length(): number {
    return this.pts.length;
  }

  /** Stored points so far (for the wet layer). */
  get points(): readonly SyncPoint[] {
    return this.pts;
  }

  get tooLong(): boolean {
    return this.overflow;
  }

  /** Add a move sample (call once per coalesced event). Returns true if a point was stored. */
  add(t: ScreenTransform, s: Sample): boolean {
    const raw = toCanonical(t, s.localX, s.localY);
    if (!Number.isFinite(raw.x) || !Number.isFinite(raw.y)) return false;
    const x = Math.fround(clamp(raw.x, 0, this.page.width));
    const y = Math.fround(clamp(raw.y, 0, this.page.height));
    const pt: SyncPoint = { x, y };
    if (this.pointerType === "pen") {
      const pr = Number.isFinite(s.pressure) ? clamp(s.pressure, 0, 1) : 0;
      if (pr > 0) this.sawPressure = true;
      pt.pressure = Math.fround(pr);
    }
    const last = this.pts[this.pts.length - 1];
    if (last && last.x === pt.x && last.y === pt.y && last.pressure === pt.pressure) return false;
    if (this.pts.length >= MAX_STROKE_POINTS) {
      this.overflow = true;
      return false;
    }
    this.pts.push(pt);
    return true;
  }

  /**
   * Complete at pointerup. The up sample is used only if it carries real pressure (browsers
   * report pressure 0 on pointerup; that is not ink).
   */
  finish(t: ScreenTransform, up: Sample | null): FinishResult {
    if (up && (this.pointerType !== "pen" || up.pressure > 0)) this.add(t, up);
    if (this.overflow) return { kind: "too-long" };
    if (this.pts.length === 0) return { kind: "empty" };
    if (this.pointerType === "pen" && this.sawPressure && this.pressureProfile) {
      return { kind: "stroke", stroke: { style: "pressure-width-v1", points: this.pts.map((p) => ({ ...p })) } };
    }
    // Mouse (and a pen that never reported pressure): fixed width, no pressure samples.
    const points: SyncPoint[] = [];
    for (const p of this.pts) {
      const last = points[points.length - 1];
      if (!last || last.x !== p.x || last.y !== p.y) points.push({ x: p.x, y: p.y });
    }
    return { kind: "stroke", stroke: { style: "legacy-fixed-v1", points } };
  }
}
