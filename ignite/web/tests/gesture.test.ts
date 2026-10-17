import { describe, expect, it } from "vitest";
import { TAP_SLOP_PX, advancePan, beginPan, displacement, isTap } from "../src/authoring/gesture";

describe("tap vs pan", () => {
  it("treats movement within slop as a tap", () => {
    expect(isTap(100, 100, 100, 100)).toBe(true);
    expect(isTap(100, 100, 100 + TAP_SLOP_PX, 100)).toBe(true);
    expect(isTap(100, 100, 100 + TAP_SLOP_PX + 1, 100)).toBe(false);
  });

  it("does not arm pan until slop is exceeded", () => {
    const pan = beginPan(1, 200, 300);
    const still = advancePan(pan, 205, 302);
    expect(still.justArmed).toBe(false);
    expect(still.scrollX).toBe(0);
    expect(still.scrollY).toBe(0);
    expect(pan.armed).toBe(false);

    const armed = advancePan(pan, 200 + TAP_SLOP_PX + 5, 300);
    expect(armed.justArmed).toBe(true);
    expect(pan.armed).toBe(true);
    expect(armed.scrollX).not.toBe(0);

    const next = advancePan(pan, 200 + TAP_SLOP_PX + 25, 310);
    expect(next.justArmed).toBe(false);
    expect(next.scrollX).toBeCloseTo(-20, 5);
    expect(next.scrollY).toBeCloseTo(-10, 5);
  });

  it("displacement matches hypot", () => {
    expect(displacement(0, 0, 3, 4)).toBe(5);
  });
});
