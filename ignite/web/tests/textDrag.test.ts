import { describe, expect, it } from "vitest";
import { isTap, TAP_SLOP_PX } from "../src/authoring/gesture";
import {
  advanceTextGrab,
  beginTextGrab,
  clearSuppressedRelease,
  consumeSuppressedRelease,
  dragCanonicalDelta,
  dragPreviewDelta,
  suppressPointerRelease,
} from "../src/authoring/textDrag";
import { clampTextOrigin, editDraft, rebaseMovedText } from "../src/authoring/textDraft";
import { CANONICAL_PAGE_HEIGHT, CANONICAL_PAGE_WIDTH } from "../src/model/generated/protocolConstants";
import { layout, screenTransform } from "../src/rendering/viewport";
import type { SyncText } from "../src/model/types";

const PAGE = { width: CANONICAL_PAGE_WIDTH, height: CANONICAL_PAGE_HEIGHT };

function text(over: Partial<SyncText> = {}): SyncText {
  return {
    text_id: "t1",
    x: 1000,
    y: 2000,
    width: 5000,
    font_size: 340,
    face: "default-sans-v1",
    content: "hello",
    ...over,
  };
}

describe("text grab click-vs-drag", () => {
  it("movement below slop remains a click/tap", () => {
    const g = beginTextGrab(100, 100, 1000, 2000, 0.05);
    expect(advanceTextGrab(g, 100 + TAP_SLOP_PX - 1, 100)).toBe(false);
    expect(g.phase).toBe("armed");
  });

  it("exactly at the threshold behaves like isTap (still a tap)", () => {
    const g = beginTextGrab(100, 100, 1000, 2000, 0.05);
    // isTap uses <=, so exactly TAP_SLOP_PX along one axis must stay armed.
    expect(isTap(100, 100, 100 + TAP_SLOP_PX, 100)).toBe(true);
    expect(advanceTextGrab(g, 100 + TAP_SLOP_PX, 100)).toBe(false);
    expect(g.phase).toBe("armed");
  });

  it("movement beyond slop arms drag exactly once", () => {
    const g = beginTextGrab(100, 100, 1000, 2000, 0.05);
    expect(advanceTextGrab(g, 100 + TAP_SLOP_PX + 1, 100)).toBe(true);
    expect(g.phase).toBe("dragging");
    expect(advanceTextGrab(g, 500, 500)).toBe(false);
  });
});

describe("drag delta conversion", () => {
  it.each([{ kind: "fit" }, { kind: "zoom", factor: 2.5 }, { kind: "zoom", factor: 0.4 }] as const)(
    "converts screen delta to canonical delta at zoom %o",
    (mode) => {
      const lay = layout({ width: 1280, height: 900 }, PAGE, mode);
      const t = screenTransform(lay, 120, 300);
      const g = beginTextGrab(400, 400, 2000, 4000, t.scale);
      // A screen move of one canonical unit at this scale round-trips.
      const d = dragCanonicalDelta(g, 400 + t.scale, 400 + 2 * t.scale);
      expect(d.dx).toBeCloseTo(1, 6);
      expect(d.dy).toBeCloseTo(2, 6);
      const p = dragPreviewDelta(g, 450, 470);
      expect(p).toEqual({ dx: 50, dy: 70 });
    },
  );
});

describe("clampTextOrigin", () => {
  it("keeps the box valid at every page edge", () => {
    const w = 5000;
    expect(clampTextOrigin(-50, -50, w)).toEqual({ x: 0, y: 0 });
    const right = clampTextOrigin(CANONICAL_PAGE_WIDTH, 100, w);
    expect(right.x + w).toBeLessThanOrEqual(CANONICAL_PAGE_WIDTH);
    expect(right.x).toBe(CANONICAL_PAGE_WIDTH - w);
    const bottom = clampTextOrigin(100, CANONICAL_PAGE_HEIGHT + 5000, w);
    expect(bottom.y).toBeLessThan(CANONICAL_PAGE_HEIGHT);
    expect(bottom.y).toBeGreaterThanOrEqual(0);
    const inside = clampTextOrigin(2000, 4000, w);
    expect(inside).toEqual({ x: Math.fround(2000), y: Math.fround(4000) });
  });
});

describe("terminal-release suppression", () => {
  it("consumes exactly one release for the suppressed pointer only", () => {
    const s = new Set<number>();
    suppressPointerRelease(s, 7);
    expect(consumeSuppressedRelease(s, 42)).toBe(false);
    expect(consumeSuppressedRelease(s, 7)).toBe(true);
    expect(consumeSuppressedRelease(s, 7)).toBe(false);
    expect(s.size).toBe(0);
  });

  it("a fresh press re-arms its own pointer ID without touching others", () => {
    const s = new Set<number>();
    suppressPointerRelease(s, 7);
    suppressPointerRelease(s, 9);
    clearSuppressedRelease(s, 7);
    expect(consumeSuppressedRelease(s, 7)).toBe(false);
    expect(consumeSuppressedRelease(s, 9)).toBe(true);
  });

  it("tracks concurrent pointers independently", () => {
    const s = new Set<number>();
    suppressPointerRelease(s, 1);
    suppressPointerRelease(s, 1);
    suppressPointerRelease(s, 2);
    expect(consumeSuppressedRelease(s, 2)).toBe(true);
    expect(consumeSuppressedRelease(s, 1)).toBe(true);
    expect(consumeSuppressedRelease(s, 3)).toBe(false);
  });
});

describe("rebaseMovedText", () => {
  it("preserves a concurrent content change while replacing only x/y", () => {
    const grabTime = text({ content: "A" });
    const latest = { ...grabTime, content: "B" };
    const moved = rebaseMovedText(latest, 7777, 8888);
    expect(moved).toEqual({ ...latest, x: 7777, y: 8888 });
    expect(moved.content).toBe("B");
    expect(moved.text_id).toBe(grabTime.text_id);
    expect(moved.width).toBe(grabTime.width);
    expect(moved.font_size).toBe(grabTime.font_size);
    expect(moved.face).toBe(grabTime.face);
  });

  it("a position-only change is an update, an identical position is nothing", async () => {
    const { finishTextEdit } = await import("../src/authoring/textDraft");
    const base = text({ content: "keep" });
    const moved = editDraft(base);
    moved.current = rebaseMovedText(base, base.x + 10, base.y);
    expect(finishTextEdit(moved)).toEqual({ kind: "update", text: moved.current });
    expect(finishTextEdit(editDraft({ ...base }))).toEqual({ kind: "nothing" });
  });
});
