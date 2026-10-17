// Touch-screen gestures on the page: two-finger pinch zooms around the page point between the
// fingers; one finger pans; a pinch never authors ink (even with "Draw with touch" on).

import type { Page } from "@playwright/test";
import { canvasInfo, connect, expect, test } from "./harness";
import { enterWrite, pinch, synced, touch, writes } from "./ink";

const W = 15624;

/** Canonical page point under viewport point (x, y), from what the canvas last painted. */
async function pageAt(page: Page, x: number, y: number) {
  const { rect } = await canvasInfo(page);
  const [ox, oy, pw] = rect;
  const scale = pw / W;
  return { x: (x - ox) / scale, y: (y - oy) / scale };
}

async function open(page: Page, server: Parameters<typeof connect>[1]) {
  await server.seedFixture();
  await page.setViewportSize({ width: 1100, height: 850 });
  await connect(page, server);
  await expect.poll(async () => (await canvasInfo(page)).rendered).toBe(true);
}

test("two-finger pinch zooms in and out around the fingers; one finger pans", async ({ page, server, traffic }) => {
  await open(page, server);
  const label = page.getByTestId("zoom-label");
  await expect(label).toHaveText("100%");

  // Pinch out (spread 100 → 250 px) around a point on the page.
  const c = { x: 420, y: 300 };
  const before = await pageAt(page, c.x, c.y);
  await pinch(page, c.x, c.y, 100, 250);
  await expect.poll(async () => (await canvasInfo(page)).zoom).toBeCloseTo(2.5, 2);
  await expect(label).toHaveText("250%");
  const after = await pageAt(page, c.x, c.y);
  // The page point between the fingers stayed under them (within a canonical unit or two).
  expect(Math.abs(after.x - before.x)).toBeLessThan(W / 1000);
  expect(Math.abs(after.y - before.y)).toBeLessThan(W / 1000);

  // One finger pans.
  const scroller = page.getByTestId("page-scroller");
  const s0 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  await touch(page, "pointerdown", 7, 500, 500);
  await touch(page, "pointermove", 7, 450, 380);
  await touch(page, "pointerup", 7, 450, 380);
  const s1 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  expect(s1[0]! - s0[0]!).toBeCloseTo(50, 0);
  expect(s1[1]! - s0[1]!).toBeCloseTo(120, 0);

  // Pinch in, past the minimum: clamped at 25%.
  await pinch(page, 400, 350, 400, 20);
  await expect(label).toHaveText("25%");
  // Fit still resets; viewing never writes.
  await page.getByTestId("fit").click();
  await expect(label).toHaveText("100%");
  expect(writes(traffic)).toEqual([]);
});

test("with Draw with touch on, a pinch zooms and authors nothing; one finger still draws", async ({ page, server, traffic }) => {
  await open(page, server);
  await enterWrite(page);
  await page.getByTestId("draw-with-touch").check();

  await pinch(page, 420, 320, 120, 240);
  await expect(page.getByTestId("zoom-label")).toHaveText("200%");
  await page.waitForTimeout(1_000);
  expect(writes(traffic)).toEqual([]); // the pinch's first finger never became a stroke
  expect(await server.head()).toBe(0);

  // One finger still draws (and is saved as exactly one stroke).
  await touch(page, "pointerdown", 9, 300, 300);
  for (let i = 1; i <= 6; i++) await touch(page, "pointermove", 9, 300 + 20 * i, 300 + 8 * i);
  await touch(page, "pointerup", 9, 420, 348);
  await synced(page, server, 1);
  expect((await server.operations()).map((o) => JSON.parse(o.envelope).operation.type)).toEqual(["add_stroke"]);
});

test("a real multi-touch pinch (Chromium touch input) zooms the page", async ({ page, server, browserName }) => {
  test.skip(browserName !== "chromium", "CDP touch input is Chromium-only");
  await open(page, server);
  const box = (await page.getByTestId("page-scroller").boundingBox())!;
  const cx = box.x + 420;
  const cy = box.y + 300;
  const cdp = await page.context().newCDPSession(page);
  const points = (d: number) => [
    { x: cx - d / 2, y: cy, id: 0 },
    { x: cx + d / 2, y: cy, id: 1 },
  ];
  await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: points(100) });
  for (let i = 1; i <= 10; i++) {
    await cdp.send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: points(100 + 15 * i) });
  }
  await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  await expect(page.getByTestId("zoom-label")).toHaveText("250%");
  // The browser did not zoom the whole web page instead.
  expect(await page.evaluate(() => window.visualViewport?.scale ?? 1)).toBe(1);
});
