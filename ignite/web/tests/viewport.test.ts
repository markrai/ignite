import { describe, expect, it } from "vitest";
import { CANONICAL_PAGE_HEIGHT, CANONICAL_PAGE_WIDTH } from "../src/model/generated/protocolConstants";
import {
  canonicalAt,
  clampZoom,
  fitScale,
  layout,
  PAGE_MARGIN,
  pinchFactor,
  screenTransform,
  scrollForZoom,
  scrollToAnchor,
  toScreen,
  ZOOM_MAX,
  ZOOM_MIN,
  zoomIn,
  zoomOut,
  type ZoomMode,
} from "../src/rendering/viewport";

const page = { width: CANONICAL_PAGE_WIDTH, height: CANONICAL_PAGE_HEIGHT };
const aspect = page.width / page.height;

describe("canonical page → viewport", () => {
  it("fit uses min(width, height) scale and never clips", () => {
    for (const avail of [
      { width: 1600, height: 900 },
      { width: 390, height: 700 },
      { width: 1024, height: 1366 },
      { width: 3000, height: 400 },
    ]) {
      const l = layout(avail, page, { kind: "fit" });
      const s = fitScale(avail, page);
      expect(l.scale).toBe(s);
      expect(s).toBe(Math.min((avail.width - 2 * PAGE_MARGIN) / page.width, (avail.height - 2 * PAGE_MARGIN) / page.height));
      expect(l.pageWidth).toBeLessThanOrEqual(avail.width - 2 * PAGE_MARGIN + 1e-9);
      expect(l.pageHeight).toBeLessThanOrEqual(avail.height - 2 * PAGE_MARGIN + 1e-9);
      expect(l.pageWidth / l.pageHeight).toBeCloseTo(aspect, 12); // no distortion
      expect(l.content).toEqual(avail); // nothing to scroll in Fit
      // centred
      expect(l.pageX).toBeCloseTo((avail.width - l.pageWidth) / 2, 9);
      expect(l.pageY).toBeCloseTo((avail.height - l.pageHeight) / 2, 9);
    }
  });

  it("maps canonical corners exactly onto the page rectangle", () => {
    const l = layout({ width: 800, height: 1000 }, page, { kind: "fit" });
    const t = screenTransform(l, 0, 0);
    expect(toScreen(t, 0, 0)).toEqual([l.pageX, l.pageY]);
    const [x, y] = toScreen(t, page.width, page.height);
    expect(x).toBeCloseTo(l.pageX + l.pageWidth, 9);
    expect(y).toBeCloseTo(l.pageY + l.pageHeight, 9);
  });

  it("re-fits on resize (no stale zoom)", () => {
    const a = layout({ width: 1200, height: 900 }, page, { kind: "fit" });
    const b = layout({ width: 500, height: 900 }, page, { kind: "fit" });
    expect(b.scale).toBeLessThan(a.scale);
    expect(b.pageWidth).toBeLessThanOrEqual(500 - 2 * PAGE_MARGIN + 1e-9);
  });
});

describe("zoom", () => {
  it("is clamped to 25%..400% of Fit", () => {
    expect(clampZoom(0.01)).toBe(ZOOM_MIN);
    expect(clampZoom(99)).toBe(ZOOM_MAX);
    let m: ZoomMode = { kind: "fit" };
    for (let i = 0; i < 30; i++) m = zoomIn(m);
    expect(m).toEqual({ kind: "zoom", factor: ZOOM_MAX });
    for (let i = 0; i < 60; i++) m = zoomOut(m);
    expect(m).toEqual({ kind: "zoom", factor: ZOOM_MIN });
  });

  it("steps from Fit and back to exactly 100%", () => {
    const up = zoomIn({ kind: "fit" });
    expect(up).toEqual({ kind: "zoom", factor: 1.25 });
    expect(zoomOut(up)).toEqual({ kind: "zoom", factor: 1 });
  });

  it("zoomed content becomes scrollable and the page stays aspect-correct", () => {
    const avail = { width: 800, height: 600 };
    const l = layout(avail, page, { kind: "zoom", factor: 3 });
    expect(l.content.width).toBeGreaterThan(avail.width);
    expect(l.content.height).toBeGreaterThan(avail.height);
    expect(l.pageWidth / l.pageHeight).toBeCloseTo(aspect, 12);
  });

  it("keeps the canonical point under the viewport centre across zoom", () => {
    const avail = { width: 800, height: 600 };
    const a = layout(avail, page, { kind: "zoom", factor: 3 });
    const scroll = { left: 300, top: 500 }; // within a's scroll range
    expect(scroll.left).toBeLessThan(a.content.width - avail.width);
    expect(scroll.top).toBeLessThan(a.content.height - avail.height);
    const b = layout(avail, page, { kind: "zoom", factor: 4 });
    const s = scrollForZoom(a, b, avail, scroll.left, scroll.top);
    const centreBefore = [(scroll.left + 400 - a.pageX) / a.scale, (scroll.top + 300 - a.pageY) / a.scale];
    const centreAfter = [(s.left + 400 - b.pageX) / b.scale, (s.top + 300 - b.pageY) / b.scale];
    expect(centreAfter[0]).toBeCloseTo(centreBefore[0]!, 6);
    expect(centreAfter[1]).toBeCloseTo(centreBefore[1]!, 6);
  });
});

describe("pinch zoom math", () => {
  const page = { width: 15624, height: 20832 };
  const avail = { width: 800, height: 600 };

  it("the factor follows the finger spread and is clamped", () => {
    expect(pinchFactor(1, 100, 200)).toBe(2);
    expect(pinchFactor(2, 200, 100)).toBe(1);
    expect(pinchFactor(1, 100, 10_000)).toBe(ZOOM_MAX);
    expect(pinchFactor(1, 100, 1)).toBe(ZOOM_MIN);
    expect(pinchFactor(1.5, 0, 50)).toBe(1.5); // degenerate spread: unchanged
  });

  it("the page point between the fingers stays under them across the zoom", () => {
    const before = layout(avail, page, { kind: "zoom", factor: 2 });
    const scroll = { left: 40, top: 300 }; // anchor mid-page (away from clamped edges)
    const focal = { x: 300, y: 250 };
    const anchor = canonicalAt(before, scroll.left, scroll.top, focal.x, focal.y);
    const after = layout(avail, page, { kind: "zoom", factor: 3 });
    const s = scrollToAnchor(after, avail, anchor, focal);
    const back = canonicalAt(after, s.left, s.top, focal.x, focal.y);
    expect(back.x).toBeCloseTo(anchor.x, 6);
    expect(back.y).toBeCloseTo(anchor.y, 6);
    // Moving fingers pan: the same page point follows the new midpoint.
    const moved = scrollToAnchor(after, avail, anchor, { x: 350, y: 200 });
    const p = canonicalAt(after, moved.left, moved.top, 350, 200);
    expect(p.x).toBeCloseTo(anchor.x, 6);
    expect(p.y).toBeCloseTo(anchor.y, 6);
  });

  it("scroll stays inside the content", () => {
    const l = layout(avail, page, { kind: "zoom", factor: 2 });
    const s = scrollToAnchor(l, avail, { x: 0, y: 0 }, { x: 700, y: 500 });
    expect(s).toEqual({ left: 0, top: 0 });
    const e = scrollToAnchor(l, avail, { x: page.width, y: page.height }, { x: 0, y: 0 });
    expect(e.left).toBe(l.content.width - avail.width);
    expect(e.top).toBe(l.content.height - avail.height);
  });
});
