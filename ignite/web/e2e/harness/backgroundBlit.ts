// Chromium pixel harness: exercises production `renderPage` against a 1px Gray8 rule.
// Bundled as an IIFE by the Playwright spec (see backgroundSmoothing.spec.ts).

import type { SnapshotPage } from "../../src/model/types";
import { decodeToCanvas, gray8ToRgba } from "../../src/rendering/background";
import { renderPage } from "../../src/rendering/renderPage";

const BG_W = 32;
const BG_H = 32;
/** Page-space size (10× the Gray8 lattice) — same stretch path as production, on-canvas. */
const PAGE_W = 320;
const PAGE_H = 320;
const MARK_Y = 16;
const MARK = 0x80;
const PAPER = 0xff;

const RENDER = {
  profile_version: 2 as const,
  ink_byte: 0,
  legacy_fixed_radius: 2,
  transform_id: "scribe1-portrait-identity-v1",
  rasterizer_id: "scribe-raster-v2-pressure",
  pressure: {
    q_r1_max: 820,
    q_r2_max: 2866,
    q_r3_max: 3685,
    min_radius: 1,
    max_radius: 4,
    missing_fallback_radius: 2,
  },
};

function ruledGray8(): Uint8Array {
  const bytes = new Uint8Array(BG_W * BG_H).fill(PAPER);
  for (let x = 0; x < BG_W; x++) bytes[MARK_Y * BG_W + x] = MARK;
  return bytes;
}

function pageBlob(byteLen: number, sha: string): SnapshotPage {
  return {
    page_id: "01a14ab8-0000-7000-8000-0000000000bg",
    background: {
      kind: "blob",
      sha256: sha,
      byte_len: byteLen,
      format: { encoding: "gray8", width: BG_W, height: BG_H, stride: BG_W },
    },
    render: RENDER,
    strokes: [
      {
        stroke_id: "01a14ab8-0000-7000-8000-0000000000s1",
        root_id: "01a14ab8-0000-7000-8000-0000000000s1",
        style: "legacy-fixed-v1",
        points: [
          { x: 100, y: 100 },
          { x: 200, y: 100 },
        ],
      },
    ],
    texts: [],
    links: [],
  };
}

/** Count channel samples that are neither paper nor the intended mark (soft fringe). */
function intermediateCount(column: number[]): number {
  return column.filter((v) => v !== PAPER && v !== MARK).length;
}

function sampleColumn(ctx: CanvasRenderingContext2D, x: number, h: number): number[] {
  const out: number[] = [];
  for (let y = 0; y < h; y++) {
    const d = ctx.getImageData(x, y, 1, 1).data;
    out.push(d[0]!);
  }
  return out;
}

/** Diagnostic: same blit geometry as renderPage, but with bilinear smoothing forced on. */
function smoothedColumn(image: CanvasImageSource, destW: number, destH: number, dpr: number, scale: number): number[] {
  const c = document.createElement("canvas");
  c.width = Math.round(destW * dpr * scale);
  c.height = Math.round(destH * dpr * scale);
  const ctx = c.getContext("2d")!;
  ctx.setTransform(dpr * scale, 0, 0, dpr * scale, 0, 0);
  ctx.fillStyle = "#ffffff";
  ctx.fillRect(0, 0, destW, destH);
  ctx.imageSmoothingEnabled = true;
  ctx.drawImage(image, 0, 0, BG_W, BG_H, 0, 0, destW, destH);
  const markDeviceY = Math.floor(((MARK_Y + 0.5) / BG_H) * destH * dpr * scale);
  return sampleColumn(ctx, Math.floor(c.width / 2), c.height).slice(
    Math.max(0, markDeviceY - 40),
    Math.min(c.height, markDeviceY + 40),
  );
}

async function runRenderPage(scale: number, dpr: number): Promise<{
  drawImageCount: number;
  intermediates: number;
  markBand: number[];
  strokeDrawn: boolean;
  markAtExpectedY: boolean;
}> {
  const bytes = ruledGray8();
  const image = await decodeToCanvas(gray8ToRgba(bytes, BG_W, BG_H, BG_W), BG_W, BG_H);
  const page = pageBlob(bytes.length, "0".repeat(64));
  const pageSize = { width: PAGE_W, height: PAGE_H };

  const cssW = 640;
  const cssH = 640;
  const c = document.createElement("canvas");
  c.width = Math.round(cssW * dpr);
  c.height = Math.round(cssH * dpr);
  const ctx = c.getContext("2d")!;

  let drawImageCount = 0;
  const orig = ctx.drawImage.bind(ctx);
  ctx.drawImage = ((...args: Parameters<CanvasRenderingContext2D["drawImage"]>) => {
    drawImageCount++;
    return orig(...args);
  }) as typeof ctx.drawImage;

  renderPage(ctx, {
    page,
    pageSize,
    background: { width: BG_W, height: BG_H, image },
    transform: { scale, offsetX: 0, offsetY: 0 },
    viewport: { width: cssW, height: cssH, dpr },
  });

  // Mark row center in page space → device px (Fit and zoom share this mapping).
  const markPageY = ((MARK_Y + 0.5) / BG_H) * pageSize.height;
  const markDeviceY = Math.floor(markPageY * dpr * scale);
  const x = Math.floor((pageSize.width / 2) * dpr * scale);
  const band = sampleColumn(ctx, x, c.height).slice(
    Math.max(0, markDeviceY - 40),
    Math.min(c.height, markDeviceY + 40),
  );
  const atMark = ctx.getImageData(x, markDeviceY, 1, 1).data[0]!;

  // Ink at (100,100) should have been stroked after the background.
  const inkX = Math.floor(100 * dpr * scale);
  const inkY = Math.floor(100 * dpr * scale);
  const ink = ctx.getImageData(inkX, inkY, 1, 1).data;
  const strokeDrawn = ink[0]! < 40 && ink[1]! < 40 && ink[2]! < 40;

  return {
    drawImageCount,
    intermediates: intermediateCount(band),
    markBand: band,
    strokeDrawn,
    markAtExpectedY: atMark === MARK,
  };
}

export type BlitResult = {
  /** Synthetic CTM scale=1 (not viewport Fit from `layout()`). */
  scale1x: Awaited<ReturnType<typeof runRenderPage>>;
  /** Synthetic CTM scale=1.5 (not ZoomMode from the app chrome). */
  scale1_5x: Awaited<ReturnType<typeof runRenderPage>>;
  smoothedIntermediates: number;
};

/** Entry for the Playwright IIFE global `BgBlit.run()`. */
export async function run(): Promise<BlitResult> {
  const bytes = ruledGray8();
  const image = await decodeToCanvas(gray8ToRgba(bytes, BG_W, BG_H, BG_W), BG_W, BG_H);
  // Large upscale (same qualitative path as panel→page) so bilinear fringes are obvious.
  const smoothed = smoothedColumn(image, PAGE_W * 8, PAGE_H * 8, 1, 1);

  // Two synthetic scales; both keep the mark on the 640 CSS-px canvas.
  const scale1x = await runRenderPage(1, 2);
  const scale1_5x = await runRenderPage(1.5, 2);

  return {
    scale1x,
    scale1_5x,
    smoothedIntermediates: intermediateCount(smoothed),
  };
}
