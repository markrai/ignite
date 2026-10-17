// Dark mode: the top-bar toggle (left of Sign out / Disconnect) switches the theme, the page
// surround follows it while the page stays white paper, and the choice survives a reload.

import { canvasInfo, connect, expect, test } from "./harness";

async function pixel(page: import("@playwright/test").Page, x: number, y: number): Promise<[number, number, number]> {
  return page.getByTestId("page-canvas").evaluate(
    (c: HTMLCanvasElement, [px, py]) => {
      const d = c.getContext("2d")!.getImageData(Math.round(px! * devicePixelRatio), Math.round(py! * devicePixelRatio), 1, 1).data;
      return [d[0]!, d[1]!, d[2]!] as [number, number, number];
    },
    [x, y],
  );
}

/** Mean 0–255 brightness of a PNG screenshot, decoded by the browser itself. */
async function meanBrightness(page: import("@playwright/test").Page, png: Buffer): Promise<number> {
  return page.evaluate(async (b64) => {
    const img = new Image();
    img.src = `data:image/png;base64,${b64}`;
    await img.decode();
    const c = document.createElement("canvas");
    c.width = img.width;
    c.height = img.height;
    const ctx = c.getContext("2d")!;
    ctx.drawImage(img, 0, 0);
    const d = ctx.getImageData(0, 0, c.width, c.height).data;
    let sum = 0;
    for (let i = 0; i < d.length; i += 4) sum += (d[i]! + d[i + 1]! + d[i + 2]!) / 3;
    return sum / (d.length / 4);
  }, png.toString("base64"));
}

/** The remembered choice, read the way the app reads it (its own tiny database). */
async function saved(page: import("@playwright/test").Page): Promise<unknown> {
  return page.evaluate(
    () =>
      new Promise((res) => {
        const r = indexedDB.open("scribe-web-prefs", 1);
        r.onupgradeneeded = () => r.result.createObjectStore("prefs");
        r.onsuccess = () => {
          const g = r.result.transaction("prefs").objectStore("prefs").get("theme");
          g.onsuccess = () => {
            res(g.result ?? null);
            r.result.close();
          };
        };
      }),
  );
}

test("theme toggle sits left of the disconnect button, themes the app, keeps paper white, persists", async ({ page, server }) => {
  await page.emulateMedia({ colorScheme: "light" });
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  await expect.poll(async () => (await canvasInfo(page)).rendered).toBe(true);

  // Placement: the toggle is the top bar's element immediately before the disconnect button.
  const order = await page.locator("header.topbar > *").evaluateAll((els) => els.map((e) => e.getAttribute("data-testid")));
  expect(order.indexOf("theme-toggle")).toBe(order.indexOf("disconnect") - 1);

  const html = page.locator("html");
  await expect(html).toHaveAttribute("data-theme", "light");
  const bodyBg = () => page.evaluate(() => getComputedStyle(document.body).backgroundColor);
  const lightBg = await bodyBg();

  await page.getByTestId("theme-toggle").click();
  await expect(html).toHaveAttribute("data-theme", "dark");
  expect(await bodyBg()).not.toBe(lightBg);

  // Pages are inverted on screen (dark paper, light ink) — display only: the rendered pixels
  // are the same white paper as in light mode, so nothing about the notebook changes.
  const filter = (id: string) => page.getByTestId(id).evaluate((c) => getComputedStyle(c).filter);
  expect(await filter("page-canvas")).toContain("invert(0.92)");
  expect(await filter("wet-canvas")).toContain("invert(0.92)");
  const [x, y, w] = (await canvasInfo(page)).rect;
  const paper = await pixel(page, x + w - 4, y + 4);
  expect(paper.every((c) => c > 240)).toBe(true);
  // What the eye sees: a screenshot of the page area is dark paper.
  const box = (await page.getByTestId("page-canvas").boundingBox())!;
  const shot = await page.screenshot({ clip: { x: box.x + x + w - 40, y: box.y + y + 40, width: 20, height: 20 } });
  expect(await meanBrightness(page, shot)).toBeLessThan(40);
  // …on a neutral dark-grey surround (not a tinted one).
  const desk = await page.screenshot({ clip: { x: box.x + 2, y: box.y + 2, width: 10, height: 10 } });
  const deskLevel = await meanBrightness(page, desk);
  expect(deskLevel).toBeGreaterThan(25);
  expect(deskLevel).toBeLessThan(70);

  // Remembered across a reload.
  await expect.poll(() => saved(page)).toBe("dark");
  await page.reload();
  await expect(html).toHaveAttribute("data-theme", "dark");
  await page.getByTestId("theme-toggle").click();
  await expect(html).toHaveAttribute("data-theme", "light");
  expect(await filter("page-canvas")).toBe("none");
  await expect.poll(() => saved(page)).toBe("light");
  await page.reload();
  await expect(html).toHaveAttribute("data-theme", "light");
});

test("with no saved choice the theme follows the system setting", async ({ page, server }) => {
  await page.emulateMedia({ colorScheme: "dark" });
  await server.seedFixture();
  await connect(page, server);
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
});
