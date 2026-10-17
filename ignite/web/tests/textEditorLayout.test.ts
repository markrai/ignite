import { describe, expect, it } from "vitest";
import { fitTextEditorHeight } from "../src/authoring/textEditorLayout";

describe("fitTextEditorHeight", () => {
  it("uses at least one line and grows to the measured content height", () => {
    expect(fitTextEditorHeight(10, 24, 200)).toEqual({ height: 24, maxHeight: 200, overflowY: "hidden" });
    expect(fitTextEditorHeight(96, 24, 200)).toEqual({ height: 96, maxHeight: 200, overflowY: "hidden" });
  });

  it("caps at the remaining paper and enables internal scrolling", () => {
    expect(fitTextEditorHeight(240, 24, 80)).toEqual({ height: 80, maxHeight: 80, overflowY: "auto" });
  });

  it("never produces a negative visible height at the page bottom", () => {
    expect(fitTextEditorHeight(24, 24, -5)).toEqual({ height: 1, maxHeight: 1, overflowY: "auto" });
  });
});
