// Canvas 2D page renderer: renderPage(ctx, page, background, transform).
//
// Draw order: desk → paper (white, or the verified background stretched over the full
// canonical page) → strokes in exact snapshot order. Strokes are vectors (never server
// screenshots). Widths/radii come from `strokePlan` (sealed profile); only antialiasing
// differs from the E Ink raster.

import type { SnapshotPage } from "../model/types";
import type { DecodedBackground } from "./background";
import type { ScreenTransform } from "./viewport";
import { checkRenderProfile, planStroke, type DrawRun } from "./strokePlan";

/** The subset of CanvasRenderingContext2D the renderer uses (fakeable in tests). */
export interface Ctx2D {
  fillStyle: string | CanvasGradient | CanvasPattern;
  strokeStyle: string | CanvasGradient | CanvasPattern;
  lineWidth: number;
  lineCap: CanvasLineCap;
  lineJoin: CanvasLineJoin;
  imageSmoothingEnabled: boolean;
  save(): void;
  restore(): void;
  setTransform(a: number, b: number, c: number, d: number, e: number, f: number): void;
  fillRect(x: number, y: number, w: number, h: number): void;
  beginPath(): void;
  rect(x: number, y: number, w: number, h: number): void;
  clip(): void;
  moveTo(x: number, y: number): void;
  lineTo(x: number, y: number): void;
  arc(x: number, y: number, r: number, a0: number, a1: number): void;
  stroke(): void;
  fill(): void;
  drawImage(image: CanvasImageSource, sx: number, sy: number, sw: number, sh: number, dx: number, dy: number, dw: number, dh: number): void;
}

export const DESK_COLOR = "#d9d6cf";
/** Dark mode: a neutral grey that the page inversion (styles.css) shows as a dark surround. */
export const DARK_DESK_COLOR = "#e6e6e6";

export interface RenderInput {
  page: SnapshotPage;
  pageSize: { width: number; height: number };
  /** Required when `page.background.kind === "blob"`; `null` there is a caller bug. */
  background: DecodedBackground | null;
  transform: ScreenTransform;
  /** Viewport size in CSS px and device pixel ratio of the backing store. */
  viewport: { width: number; height: number; dpr: number };
  /** Area around the page (default `DESK_COLOR`). */
  deskColor?: string;
}

export interface RenderStats {
  strokes: number;
  runs: number;
}

function ink(inkByte: number): string {
  return inkByte === 0xff ? "#ffffff" : "#000000";
}

/** Plans every stroke up front so a profile problem fails before anything is painted. */
export function planPage(page: SnapshotPage): DrawRun[][] {
  checkRenderProfile(page.render);
  return page.strokes.map((s) => planStroke(s, page.render));
}

export function renderPage(ctx: Ctx2D, input: RenderInput, plans: DrawRun[][] = planPage(input.page)): RenderStats {
  const { page, pageSize, background, transform: t, viewport } = input;
  ctx.save();
  ctx.setTransform(viewport.dpr, 0, 0, viewport.dpr, 0, 0);
  ctx.fillStyle = input.deskColor ?? DESK_COLOR;
  ctx.fillRect(0, 0, viewport.width, viewport.height);

  // Page space → CSS px: x' = offsetX + x·scale.
  ctx.setTransform(viewport.dpr * t.scale, 0, 0, viewport.dpr * t.scale, viewport.dpr * t.offsetX, viewport.dpr * t.offsetY);
  ctx.beginPath();
  ctx.rect(0, 0, pageSize.width, pageSize.height);
  ctx.clip();
  ctx.fillStyle = "#ffffff";
  ctx.fillRect(0, 0, pageSize.width, pageSize.height);
  if (page.background.kind === "blob") {
    if (!background) throw new Error("blob background not loaded");
    ctx.imageSmoothingEnabled = true;
    ctx.drawImage(background.image, 0, 0, background.width, background.height, 0, 0, pageSize.width, pageSize.height);
  }

  const runs = paintRuns(ctx, ink(page.render.ink_byte), plans);
  ctx.restore();
  return { strokes: plans.length, runs };
}

/** Paint planned runs in the current (page-space) transform. Returns the run count. */
function paintRuns(ctx: Ctx2D, color: string, plans: DrawRun[][]): number {
  ctx.strokeStyle = color;
  ctx.fillStyle = color;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  let runs = 0;
  for (const strokeRuns of plans) {
    for (const run of strokeRuns) {
      runs++;
      const p0 = run.points[0]!;
      if (run.points.length === 1) {
        ctx.beginPath();
        ctx.arc(p0.x, p0.y, run.width / 2, 0, Math.PI * 2);
        ctx.fill();
        continue;
      }
      ctx.lineWidth = run.width;
      ctx.beginPath();
      ctx.moveTo(p0.x, p0.y);
      for (let i = 1; i < run.points.length; i++) ctx.lineTo(run.points[i]!.x, run.points[i]!.y);
      ctx.stroke();
    }
  }
  return runs;
}

/**
 * S3B wet layer: paint strokes on a transparent canvas with the same page transform and brush
 * as `renderPage` (no desk/paper; clipped to the page). Used for in-progress ink only.
 */
export function paintInk(
  ctx: Ctx2D,
  input: Omit<RenderInput, "page" | "background"> & { render: SnapshotPage["render"] },
  plans: DrawRun[][],
): void {
  const { pageSize, transform: t, viewport } = input;
  ctx.save();
  ctx.setTransform(viewport.dpr * t.scale, 0, 0, viewport.dpr * t.scale, viewport.dpr * t.offsetX, viewport.dpr * t.offsetY);
  ctx.beginPath();
  ctx.rect(0, 0, pageSize.width, pageSize.height);
  ctx.clip();
  paintRuns(ctx, ink(input.render.ink_byte), plans);
  ctx.restore();
}
