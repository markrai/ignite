// Chromium pixel regression: production `renderPage` must blit Gray8 backgrounds with
// nearest-neighbor sampling so thin marks do not grow a soft “doubled” fringe.

import { test, expect } from "@playwright/test";
import { build } from "vite";
import { mkdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { WEB_DIR } from "./harness";

const OUT = join(WEB_DIR, "e2e/harness/dist");
const ENTRY = resolve(WEB_DIR, "e2e/harness/backgroundBlit.ts");

test.describe("Gray8 background blit", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "Chromium canvas rasterization");

  test("renderPage nearest-neighbor eliminates soft fringes; smoothed path still shows them", async ({
    page,
  }) => {
    mkdirSync(OUT, { recursive: true });
    await build({
      configFile: false,
      root: WEB_DIR,
      logLevel: "error",
      build: {
        outDir: OUT,
        emptyOutDir: true,
        minify: false,
        lib: {
          entry: ENTRY,
          name: "BgBlit",
          formats: ["iife"],
          fileName: () => "backgroundBlit.js",
        },
      },
    });

    const jsPath = join(OUT, "backgroundBlit.js");
    await page.setContent("<!doctype html><html><body></body></html>");
    await page.addScriptTag({ path: jsPath });

    type Pass = {
      drawImageCount: number;
      intermediates: number;
      markBand: number[];
      strokeDrawn: boolean;
      markAtExpectedY: boolean;
    };
    // Synthetic CTM scales (not viewport Fit / ZoomMode from layout()).
    type Result = { scale1x: Pass; scale1_5x: Pass; smoothedIntermediates: number };

    const result = (await page.evaluate(() => {
      const g = (globalThis as unknown as { BgBlit?: { run: () => Promise<Result> } }).BgBlit;
      if (!g?.run) throw new Error("BgBlit harness missing");
      return g.run();
    })) as Result;

    // Root-cause evidence: bilinear upscale of a 1px mark invents intermediate grays.
    expect(result.smoothedIntermediates).toBeGreaterThan(0);

    // Production path: one background drawImage, no soft fringe, strokes still after paper.
    for (const label of ["scale1x", "scale1_5x"] as const) {
      const r = result[label];
      expect(r.drawImageCount, label).toBe(1);
      expect(r.intermediates, label).toBe(0);
      expect(r.markBand.includes(0x80), `${label} contains mark`).toBe(true);
      expect(r.markBand.includes(0xff), `${label} contains paper`).toBe(true);
      expect(r.markAtExpectedY, `${label} mark aligned`).toBe(true);
      expect(r.strokeDrawn, label).toBe(true);
    }
  });
});
