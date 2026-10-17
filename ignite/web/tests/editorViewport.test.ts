import { afterEach, describe, expect, it, vi } from "vitest";
import { keepEditorInVisualViewport } from "../src/authoring/editorViewport";

describe("keepEditorInVisualViewport", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("scrolls the scroller when the editor sits below the visual viewport", () => {
    const scrollBy = vi.fn();
    const scroller = { scrollBy } as unknown as HTMLElement;
    const editor = {
      getBoundingClientRect: () => ({ top: 500, bottom: 700, left: 40, right: 200, width: 160, height: 200 }),
    } as unknown as HTMLElement;

    vi.stubGlobal("visualViewport", { offsetTop: 0, offsetLeft: 0, height: 400, width: 390 });
    keepEditorInVisualViewport(editor, scroller);
    expect(scrollBy).toHaveBeenCalled();
    const [dx, dy] = scrollBy.mock.calls[0]!;
    expect(dx).toBe(0);
    expect(dy).toBeGreaterThan(0);
  });

  it("falls back to window.innerHeight when visualViewport is missing", () => {
    const scrollBy = vi.fn();
    const scroller = { scrollBy } as unknown as HTMLElement;
    const editor = {
      getBoundingClientRect: () => ({ top: 10, bottom: 900, left: 10, right: 100, width: 90, height: 890 }),
    } as unknown as HTMLElement;

    vi.stubGlobal("visualViewport", null);
    vi.stubGlobal("innerHeight", 600);
    vi.stubGlobal("innerWidth", 390);
    keepEditorInVisualViewport(editor, scroller);
    expect(scrollBy).toHaveBeenCalled();
    expect(scrollBy.mock.calls[0]![1]).toBeGreaterThan(0);
  });
});
