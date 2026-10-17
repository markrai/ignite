import { describe, expect, it } from "vitest";
import { REFERENCE_RASTER } from "../src/model/generated/protocolConstants";
import type { RenderProfileRef, SnapshotPage, SyncStroke } from "../src/model/types";
import { validateSnapshot } from "../src/model/validate";
import {
  bresenhamSteps,
  canonicalWidthForRadius,
  quantizePressure,
  radiusForPressure,
  segmentRadii,
  toPanelPixel,
} from "../src/rendering/brush";
import { renderPage, type Ctx2D } from "../src/rendering/renderPage";
import { checkRenderProfile, planStroke, UnsupportedRenderProfile } from "../src/rendering/strokePlan";
import { brush, snapshotJson } from "./fixtures";

const snap = validateSnapshot(snapshotJson);
const page1 = snap.notebooks[0]!.pages[0]!;
const render = page1.render;
const curve = render.pressure!;

describe("pressure mapping pinned to the sealed Kindle curve (Rust fixture)", () => {
  it("uses the same curve the snapshot carries", () => {
    expect(curve).toEqual(brush.curve);
    expect(render.legacy_fixed_radius).toBe(brush.legacy_fixed_radius);
  });

  it("quantizes and bands every sampled pressure identically (missing, min, boundaries, max)", () => {
    expect(brush.pressure.length).toBeGreaterThan(40);
    for (const c of brush.pressure) {
      const p = c.p ?? undefined;
      if (p !== undefined) expect(quantizePressure(p), `q(${p})`).toBe(c.q);
      expect(radiusForPressure(curve, p), `r(${p})`).toBe(c.radius);
    }
    expect(radiusForPressure(curve, undefined)).toBe(curve.missing_fallback_radius);
    expect(radiusForPressure(curve, 0)).toBe(1);
    expect(radiusForPressure(curve, 1)).toBe(4);
  });

  it("projects canonical points to the reference raster like the Kindle (f32 floor/clamp)", () => {
    for (const c of brush.panel_pixel) expect(toPanelPixel(c.x, c.y), `${c.x},${c.y}`).toEqual([c.px, c.py]);
  });

  it("walks pressure segments with identical per-step radii", () => {
    for (const s of brush.segments) {
      const from = toPanelPixel(s.from.x, s.from.y);
      const to = toPanelPixel(s.to.x, s.to.y);
      expect(from).toEqual(s.from_px);
      expect(to).toEqual(s.to_px);
      expect(bresenhamSteps(from[0], from[1], to[0], to[1])).toBe(s.steps);
      expect(segmentRadii(curve, from, s.from.p ?? undefined, to, s.to.p ?? undefined)).toEqual(s.radii);
    }
  });

  it("logical width is the (2r+1)-pixel square brush in canonical units", () => {
    expect(canonicalWidthForRadius(2)).toBeCloseTo(5 / REFERENCE_RASTER.scaleX, 9);
    expect(canonicalWidthForRadius(1)).toBeCloseTo(3 * 8.4, 3);
  });
});

function pressureStroke(points: [number, number, number | undefined][]): SyncStroke {
  return {
    stroke_id: "019a8ef2-0000-7000-8000-000000000001",
    root_id: "019a8ef2-0000-7000-8000-000000000001",
    style: "pressure-width-v1",
    points: points.map(([x, y, p]) => (p === undefined ? { x, y } : { x, y, pressure: p })),
  };
}

describe("stroke planning", () => {
  it("legacy-fixed-v1: one constant-width run at the profile radius", () => {
    const legacy = page1.strokes.find((s) => s.style === "legacy-fixed-v1")!;
    const runs = planStroke(legacy, render);
    expect(runs).toHaveLength(1);
    expect(runs[0]!.radius).toBe(render.legacy_fixed_radius);
    expect(runs[0]!.points).toEqual(legacy.points.map((p) => ({ x: p.x, y: p.y })));
  });

  it("pressure-width-v1: widths follow the per-step radius, contiguous with no gaps", () => {
    const seg = brush.segments[0]!;
    const runs = planStroke(
      pressureStroke([
        [seg.from.x, seg.from.y, seg.from.p ?? undefined],
        [seg.to.x, seg.to.y, seg.to.p ?? undefined],
      ]),
      render,
    );
    // Run radii = the distinct consecutive values of the Kindle's per-step radii.
    const distinct = seg.radii.filter((r, i) => i === 0 || r !== seg.radii[i - 1]);
    expect(runs.map((r) => r.radius)).toEqual(distinct);
    expect(runs.length).toBeGreaterThan(1); // light → firm is visibly varying
    for (let i = 1; i < runs.length; i++) {
      expect(runs[i]!.points[0]).toEqual(runs[i - 1]!.points.at(-1)); // shared joint
    }
    expect(runs[0]!.points[0]).toEqual({ x: seg.from.x, y: seg.from.y });
    expect(runs.at(-1)!.points.at(-1)).toEqual({ x: seg.to.x, y: seg.to.y });
  });

  it("missing pressure uses the fallback radius for the whole segment", () => {
    const runs = planStroke(pressureStroke([[3000, 3000, undefined], [4000, 3500, 0.9]]), render);
    expect(runs.map((r) => r.radius)).toEqual([curve.missing_fallback_radius]);
  });

  it("a single sample is one dot at its own radius", () => {
    const runs = planStroke(pressureStroke([[100, 100, 1]]), render);
    expect(runs).toEqual([{ radius: 4, width: canonicalWidthForRadius(4), points: [{ x: 100, y: 100 }] }]);
  });

  it("unknown render profiles are refused (visible page error)", () => {
    const bad: RenderProfileRef = { ...render, rasterizer_id: "future-raster" };
    expect(() => checkRenderProfile(bad)).toThrow(UnsupportedRenderProfile);
    expect(() => checkRenderProfile({ ...render, transform_id: "landscape" })).toThrow(UnsupportedRenderProfile);
    expect(() => checkRenderProfile({ ...render, pressure: { ...curve, max_radius: 9 } })).toThrow(UnsupportedRenderProfile);
    checkRenderProfile(render);
  });
});

/** Records draw calls; enough to assert order, backgrounds and geometry. */
function fakeCtx() {
  const calls: { op: string; args: unknown[]; lineWidth?: number; fill?: unknown }[] = [];
  const ctx: Ctx2D & { calls: typeof calls } = {
    calls,
    fillStyle: "",
    strokeStyle: "",
    lineWidth: 1,
    lineCap: "butt",
    lineJoin: "miter",
    imageSmoothingEnabled: false,
    save: () => calls.push({ op: "save", args: [] }),
    restore: () => calls.push({ op: "restore", args: [] }),
    setTransform: (...a) => calls.push({ op: "setTransform", args: a }),
    fillRect(...a) {
      calls.push({ op: "fillRect", args: a, fill: this.fillStyle });
    },
    beginPath: () => calls.push({ op: "beginPath", args: [] }),
    rect: (...a) => calls.push({ op: "rect", args: a }),
    clip: () => calls.push({ op: "clip", args: [] }),
    moveTo: (...a) => calls.push({ op: "moveTo", args: a }),
    lineTo: (...a) => calls.push({ op: "lineTo", args: a }),
    arc: (...a) => calls.push({ op: "arc", args: a }),
    stroke() {
      calls.push({ op: "stroke", args: [], lineWidth: this.lineWidth });
    },
    fill: () => calls.push({ op: "fill", args: [] }),
    drawImage: (...a) => calls.push({ op: "drawImage", args: a }),
  };
  return ctx;
}

const size = snap.page_space;
const t = { scale: 0.05, offsetX: 10, offsetY: 20 };
const vp = { width: 900, height: 1200, dpr: 2 };

describe("renderPage", () => {
  it("blank background: white paper over the full canonical page, no image", () => {
    const ctx = fakeCtx();
    renderPage(ctx, { page: page1, pageSize: size, background: null, transform: t, viewport: vp });
    expect(ctx.calls.some((c) => c.op === "drawImage")).toBe(false);
    expect(ctx.calls.find((c) => c.op === "fillRect" && c.fill === "#ffffff")!.args).toEqual([0, 0, size.width, size.height]);
    // page transform: dpr·scale and dpr·offset
    expect(ctx.calls.filter((c) => c.op === "setTransform")[1]!.args).toEqual([0.1, 0, 0, 0.1, 20, 40]);
  });

  it("blob background is stretched over the whole page before strokes", () => {
    const page = snap.notebooks[1]!.pages[0]!;
    const img = {} as CanvasImageSource;
    const ctx = fakeCtx();
    renderPage(ctx, { page, pageSize: size, background: { width: 93, height: 124, image: img }, transform: t, viewport: vp });
    const di = ctx.calls.findIndex((c) => c.op === "drawImage");
    expect(ctx.calls[di]!.args).toEqual([img, 0, 0, 93, 124, 0, 0, size.width, size.height]);
    expect(ctx.calls.findIndex((c) => c.op === "moveTo")).toBeGreaterThan(di);
  });

  it("blob page without a loaded background is a caller error (never silent white)", () => {
    const page = snap.notebooks[1]!.pages[0]!;
    expect(() => renderPage(fakeCtx(), { page, pageSize: size, background: null, transform: t, viewport: vp })).toThrow();
  });

  it("draws strokes (incl. erase fragments) in exact snapshot order", () => {
    const ctx = fakeCtx();
    const stats = renderPage(ctx, { page: page1, pageSize: size, background: null, transform: t, viewport: vp });
    expect(stats.strokes).toBe(page1.strokes.length);
    // First path start of each stroke, in draw order, equals each stroke's first point in order.
    const starts: unknown[] = [];
    let seenForStroke = 0;
    const firstPoints = page1.strokes.map((s) => [s.points[0]!.x, s.points[0]!.y]);
    for (const c of ctx.calls) {
      if (c.op === "moveTo" || c.op === "arc") {
        const xy = c.args.slice(0, 2);
        if (seenForStroke < firstPoints.length && JSON.stringify(xy) === JSON.stringify(firstPoints[seenForStroke])) {
          starts.push(xy);
          seenForStroke++;
        }
      }
    }
    expect(starts).toEqual(firstPoints);
  });

  it("uses round caps and joins with pressure-dependent line widths", () => {
    const ctx = fakeCtx();
    renderPage(ctx, { page: page1, pageSize: size, background: null, transform: t, viewport: vp });
    expect(ctx.lineCap).toBe("round");
    expect(ctx.lineJoin).toBe("round");
    const widths = new Set(ctx.calls.filter((c) => c.op === "stroke").map((c) => c.lineWidth));
    expect(widths.size).toBeGreaterThanOrEqual(3);
  });

  it("draws nothing when the profile is unsupported (planned before painting)", () => {
    const bad: SnapshotPage = { ...page1, render: { ...page1.render, transform_id: "x" } };
    const ctx = fakeCtx();
    expect(() => renderPage(ctx, { page: bad, pageSize: size, background: null, transform: t, viewport: vp })).toThrow(UnsupportedRenderProfile);
    expect(ctx.calls).toEqual([]);
  });
});
