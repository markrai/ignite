import type { Page } from "@playwright/test";
import { canvasInfo, connect, expect, expectReadOnly, inkPixels, manifest, snapshot, test } from "./harness";

const ASPECT = 15624 / 20832;

async function expectFittedAndCentred(page: Page) {
  await expect.poll(async () => (await canvasInfo(page)).rendered).toBe(true);
  const { rect, viewport } = await canvasInfo(page);
  const [x, y, w, h] = rect;
  const [vw, vh] = viewport;
  expect(w / h).toBeCloseTo(ASPECT, 3); // no distortion
  expect(x).toBeGreaterThanOrEqual(0); // no cropping
  expect(y).toBeGreaterThanOrEqual(0);
  expect(x + w).toBeLessThanOrEqual(vw + 0.5);
  expect(y + h).toBeLessThanOrEqual(vh + 0.5);
  expect(Math.abs(x - (vw - x - w))).toBeLessThan(1.5); // centred
  expect(Math.abs(y - (vh - y - h))).toBeLessThan(1.5);
  // Fit touches one pair of sides (minus the margin).
  expect(Math.min(x, y)).toBeLessThanOrEqual(16.5);
}

test("full read-only session: discover, render, navigate, zoom, resize, live refresh", async ({ page, server, traffic }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);

  // One library → opened automatically; notebooks in canonical order with real titles.
  const items = page.getByTestId("notebook-item");
  await expect(items).toHaveCount(snapshot.notebooks.length);
  await expect(items.nth(0)).toContainText("Field Notes");
  await expect(items.nth(0)).toContainText("3 pages");
  await expect(items.nth(1)).toContainText("Sketches");
  await expect(items.nth(1)).toContainText("2 pages");
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3");

  // Page renders from canonical vectors, fitted and centred.
  await expectFittedAndCentred(page);
  const p1 = await canvasInfo(page);
  expect(p1.strokes).toBe(snapshot.notebooks[0]!.pages[0]!.strokes.length);
  expect(p1.background).toBe("blank");
  expect(await inkPixels(page)).toBeGreaterThan(500);

  // Previous / next within the notebook.
  await page.getByTestId("next").click();
  await expect(page.getByTestId("page-label")).toHaveText("2 of 3");
  await expect.poll(async () => (await canvasInfo(page)).strokes).toBe(snapshot.notebooks[0]!.pages[1]!.strokes.length);
  await page.getByTestId("next").click();
  await expect(page.getByTestId("page-label")).toHaveText("3 of 3");
  await expect(page.getByTestId("next")).toBeDisabled();
  await page.getByTestId("prev").click();
  await page.getByTestId("prev").click();
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3");

  // Change notebook → blob background verified + rendered.
  await items.nth(1).click();
  await expect(page.getByTestId("page-label")).toHaveText("1 of 2");
  await expect.poll(async () => (await canvasInfo(page)).background).toBe("blob");
  await expectFittedAndCentred(page);
  await expect(page.getByTestId("page-error")).toHaveCount(0);

  // Zoom in → larger, scrollable; zoom out; Fit restores the whole page immediately.
  const fitScale = (await canvasInfo(page)).rect[2];
  await page.getByTestId("zoom-in").click();
  await expect(page.getByTestId("zoom-label")).toHaveText("125%");
  await page.getByTestId("zoom-in").click();
  await page.getByTestId("zoom-in").click();
  await expect.poll(async () => (await canvasInfo(page)).rect[2]).toBeGreaterThan(fitScale * 1.9);
  const scroller = page.getByTestId("page-scroller");
  const scrollable = await scroller.evaluate((el) => el.scrollHeight > el.clientHeight);
  expect(scrollable).toBe(true);
  await scroller.evaluate((el) => el.scrollBy(0, 200));
  await page.getByTestId("zoom-out").click();
  await expect(page.getByTestId("zoom-label")).toHaveText("156%");
  await page.getByTestId("fit").click();
  await expect(page.getByTestId("zoom-label")).toHaveText("100%");
  await expectFittedAndCentred(page);

  // Resize narrow and wide: still fitted, centred, undistorted.
  for (const size of [
    { width: 480, height: 900 },
    { width: 1600, height: 700 },
    { width: 1024, height: 1366 },
  ]) {
    await page.setViewportSize(size);
    await expect.poll(async () => (await canvasInfo(page)).viewport[0]).toBeLessThanOrEqual(size.width);
    await page.waitForTimeout(100);
    await expectFittedAndCentred(page);
  }
  await page.setViewportSize({ width: 1280, height: 900 });

  // Live refresh (no reload): the "Kindle" (harness) appends a stroke, then erases part of it.
  await items.nth(0).click();
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3");
  const before = (await canvasInfo(page)).strokes;
  const status = page.getByTestId("status");
  await expect(status).toHaveAttribute("data-head", "0");
  await server.pushLiveOp(0);
  await expect(status).toHaveAttribute("data-head", "1", { timeout: 15_000 });
  await expect(status).toHaveAttribute("data-snapshot", manifest.snapshot_sha256_after_live_ops[0]!);
  await expect.poll(async () => (await canvasInfo(page)).strokes).toBe(before + 1);
  await server.pushLiveOp(1);
  await expect(status).toHaveAttribute("data-head", "2", { timeout: 15_000 });
  await expect(status).toHaveAttribute("data-snapshot", manifest.snapshot_sha256_after_live_ops[1]!);
  await expect.poll(async () => (await canvasInfo(page)).strokes).toBe(before + 2); // split into 2 fragments
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3"); // selection preserved

  // Gates: only GETs from the browser; ledger moved only by the harness; no console errors.
  expectReadOnly(traffic);
  expect(await server.head()).toBe(2);
  expect(traffic.consoleErrors).toEqual([]);
});

test("browser interaction cannot mutate the ledger", async ({ page, server, traffic }) => {
  await server.seedFixture();
  await connect(page, server);
  await expect(page.getByTestId("notebook-item")).toHaveCount(2);
  await expect.poll(async () => (await canvasInfo(page)).rendered).toBe(true);
  const scroller = page.getByTestId("page-scroller");
  const box = (await scroller.boundingBox())!;
  const cx = box.x + box.width / 2;
  const cy = box.y + box.height / 2;
  // Click, drag, double-click, wheel (with and without ctrl), keys: navigation only.
  await page.mouse.click(cx, cy);
  await page.mouse.move(cx - 100, cy - 100);
  await page.mouse.down();
  await page.mouse.move(cx + 150, cy + 120, { steps: 10 });
  await page.mouse.up();
  await page.mouse.dblclick(cx, cy);
  await page.mouse.wheel(0, 300);
  await page.keyboard.down("Control");
  await page.mouse.wheel(0, -200);
  await page.keyboard.up("Control");
  await page.keyboard.press("ArrowRight");
  await page.keyboard.press("ArrowLeft");
  await page.getByTestId("zoom-in").click();
  await page.getByTestId("fit").click();
  await page.getByTestId("notebook-item").nth(1).click();
  await page.getByTestId("next").click();
  // Let at least two poll cycles happen.
  await page.waitForTimeout(6_000);
  expectReadOnly(traffic);
  expect(await server.head()).toBe(0);
  const polls = traffic.requests.filter((r) => new URL(r.url).pathname === `/v1/libraries/${manifest.library_id}`);
  expect(polls.length).toBeGreaterThanOrEqual(2);
  expect(traffic.consoleErrors).toEqual([]);
});

test("wrong token stays locked and is not retried; token never in URL or localStorage", async ({ page, server, traffic }) => {
  await server.seedFixture();
  await connect(page, server, "0".repeat(64));
  await expect(page.getByTestId("connect-error")).toContainText("rejected");
  await page.waitForTimeout(3_000);
  const listCalls = traffic.requests.filter((r) => new URL(r.url).pathname === "/v1/libraries");
  expect(listCalls.length).toBe(1); // no silent retry loop
  await expect(page.getByTestId("connect-form")).toBeVisible();

  // Correct token: connected; token lives only in sessionStorage.
  await page.getByTestId("token-input").fill(server.token);
  await page.getByTestId("connect").click();
  await expect(page.getByTestId("notebook-item")).toHaveCount(2);
  for (const r of traffic.requests) expect(r.url).not.toContain(server.token);
  const storage = await page.evaluate(() => ({
    local: JSON.stringify(Object.entries(localStorage)),
    session: JSON.stringify(Object.entries(sessionStorage)),
  }));
  expect(storage.local).not.toContain(server.token);
  expect(storage.session).toContain(server.token);

  // Reload in the same tab reconnects from sessionStorage without asking again.
  await page.reload();
  await expect(page.getByTestId("notebook-item")).toHaveCount(2);

  // Disconnect clears the token and notebook state.
  await page.getByTestId("disconnect").click();
  await expect(page.getByTestId("connect-form")).toBeVisible();
  const after = await page.evaluate(() => JSON.stringify(Object.entries(sessionStorage)));
  expect(after).not.toContain(server.token);
  await expect(page.getByTestId("notebook-item")).toHaveCount(0);
  expectReadOnly(traffic);
  // Only the deliberate wrong-token attempt produced a (network) console error.
  expect(traffic.consoleErrors.filter((e) => !/401|Unauthorized/i.test(e))).toEqual([]);
});

test("no libraries → message; two libraries → chooser", async ({ page, server, traffic }) => {
  await connect(page, server);
  await expect(page.getByTestId("no-libraries")).toHaveText("No synchronized libraries");
  await server.seedFixture();
  await server.seedSecondLibrary();
  await page.getByRole("button", { name: "Disconnect" }).click();
  await connect(page, server);
  const cards = page.getByTestId("library-card");
  await expect(cards).toHaveCount(2);
  // Identified by ID + counts only (no invented library title).
  const ids = [manifest.library_id, manifest.second_library.library_id].sort();
  await expect(cards.nth(0)).toHaveAttribute("title", ids[0]!);
  await expect(cards.nth(1)).toHaveAttribute("title", ids[1]!);
  const fixtureCard = page.locator(`[data-testid="library-card"][title="${manifest.library_id}"]`);
  await expect(fixtureCard).toContainText("2 notebooks · 5 pages");
  await fixtureCard.click();
  await expect(page.getByTestId("notebook-item").nth(0)).toContainText("Field Notes");
  expectReadOnly(traffic);
  expect(traffic.consoleErrors).toEqual([]);
});

test("narrow (phone) layout: drawer sidebar, finger-sized controls, fitted page", async ({ page, server, traffic }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 390, height: 844 });
  await connect(page, server);
  await expect.poll(async () => (await canvasInfo(page)).rendered).toBe(true);
  await expectFittedAndCentred(page);
  const toggle = page.getByRole("button", { name: "Notebooks" });
  await expect(toggle).toBeVisible();
  for (const id of ["prev", "next", "fit", "zoom-in", "zoom-out"]) {
    const b = (await page.getByTestId(id).boundingBox())!;
    expect(Math.min(b.width, b.height), id).toBeGreaterThanOrEqual(43.5); // 44px (sub-pixel layout)
  }
  await toggle.click();
  await page.getByTestId("notebook-item").nth(1).click();
  await expect(page.getByTestId("page-label")).toHaveText("1 of 2");
  await expectFittedAndCentred(page);
  expectReadOnly(traffic);
  expect(traffic.consoleErrors).toEqual([]);
});
