import { randomUUID } from "node:crypto";
import type { Page } from "@playwright/test";
import { canvasInfo, connect, expect, manifest, test } from "./harness";
import { drawPen, enterType, enterWrite, envelope, mapper, opPosts, pinch, synced, touch, writes } from "./ink";
import type { SyncText } from "../src/model/types";

type TextOp = { type: string; text: SyncText; text_id: string };

async function opTypes(server: Parameters<typeof connect>[1]): Promise<string[]> {
  return (await server.operations()).map((o) => envelope(o.envelope).operation.type);
}

async function textOp(server: Parameters<typeof connect>[1], index: number): Promise<TextOp> {
  const ops = await server.operations();
  return envelope(ops[index]!.envelope).operation as unknown as TextOp;
}

async function firstPageTexts(server: Parameters<typeof connect>[1]) {
  const snap = await server.snapshot();
  return snap.snapshot.notebooks[0]!.pages[0]!.texts ?? [];
}

async function postTextUpdate(
  server: Parameters<typeof connect>[1],
  changes: Partial<Pick<SyncText, "x" | "y" | "width" | "font_size" | "content">>,
) {
  const snap = await server.snapshot();
  const notebook = snap.snapshot.notebooks[0]!;
  const page = notebook.pages[0]!;
  const current = page.texts?.[0];
  if (!current) throw new Error("expected first-page text");
  const text = { ...current, ...changes };
  const env = JSON.stringify({
    v: 1,
    operation_id: randomUUID(),
    library_id: manifest.library_id,
    replica_id: randomUUID(),
    client_seq: 1,
    operation: { type: "update_text", notebook_id: notebook.notebook_id, page_id: page.page_id, text },
  });
  const posted = await server.api("POST", `/v1/libraries/${manifest.library_id}/operations`, `{"envelopes":[${env}]}`);
  expect(posted.status).toBe(200);
  return text;
}

async function placeText(page: Page, cx: number, cy: number, content: string) {
  const m = await mapper(page);
  const c = m.toClient({ x: cx, y: cy });
  await page.mouse.click(c.x, c.y);
  const editor = page.getByTestId("text-editor");
  await expect(editor).toBeVisible();
  await editor.fill(content);
  await page.getByTestId("done").click();
}

async function expectPageText(page: Page, content: string) {
  await expect.poll(async () => (await canvasInfo(page)).rendered).toBe(true);
  await expect(page.getByTestId("page-text")).toHaveText(content);
}

async function openType(page: Page, server: Parameters<typeof connect>[1], size = { width: 1280, height: 900 }) {
  await server.seedFixture();
  await page.setViewportSize(size);
  await connect(page, server);
  await enterType(page);
}

type SyntheticTouch = "pointerdown" | "pointermove" | "pointerup" | "pointercancel" | "click";

/** Synthetic touch pointers targeted at one overlay element. Offsets in client px from its center. */
async function touchOn(page: Page, testId: string, type: SyntheticTouch, id: number, dx: number, dy: number) {
  await page.getByTestId(testId).first().evaluate(
    (el, a) => {
      const r = el.getBoundingClientRect();
      el.dispatchEvent(
        new PointerEvent(a.type, {
          pointerId: a.id,
          pointerType: "touch",
          isPrimary: false,
          bubbles: true,
          cancelable: true,
          clientX: r.left + r.width / 2 + a.dx,
          clientY: r.top + r.height / 2 + a.dy,
          buttons: a.type === "pointerup" || a.type === "click" ? 0 : 1,
        }),
      );
    },
    { type, id, dx, dy },
  );
}

/** Synthetic touch pointers targeted at the text element (arms the text grab). */
async function textTouch(page: Page, type: SyntheticTouch, id: number, dx: number, dy: number) {
  await touchOn(page, "page-text", type, id, dx, dy);
}

test("type mode places a durable text object", async ({ page, server }) => {
  await openType(page, server);
  const m = await mapper(page);
  const c = m.toClient({ x: 2000, y: 4000 });
  await page.mouse.click(c.x, c.y);
  const editor = page.getByTestId("text-editor");
  await expect(editor).toBeVisible();
  await editor.fill("hello ignite");
  await page.getByTestId("done").click();
  await synced(page, server, 1);
  const snap = await server.snapshot();
  const texts = snap.snapshot.notebooks[0]!.pages[0]!.texts ?? [];
  expect(texts).toHaveLength(1);
  expect(texts[0]!.content).toBe("hello ignite");
  expect(texts[0]!.face).toBe("default-sans-v1");
  // Firefox/WebKit quantize Playwright mouse coordinates to a CSS pixel. The
  // placement must preserve that delivered anchor rather than shifting it.
  const canonicalCssPixel = 1 / m.scale + 0.01;
  expect(Math.abs(texts[0]!.x - 2000)).toBeLessThanOrEqual(canonicalCssPixel);
  expect(Math.abs(texts[0]!.y - 4000)).toBeLessThanOrEqual(canonicalCssPixel);
  expect(texts[0]!.width).toBe(10_000);
  expect(texts[0]!.font_size).toBe(340);
  expect(texts[0]).not.toHaveProperty("height");
  await expectPageText(page, "hello ignite");
  // Remount React (token still in sessionStorage, replica in IndexedDB).
  await page.reload();
  await expectPageText(page, "hello ignite");
  // Re-auth with a cleared token store; IndexedDB replica + server snapshot must still paint.
  await page.evaluate(() => sessionStorage.clear());
  await connect(page, server);
  await expectPageText(page, "hello ignite");
});

test("active editor grows with wrapping, paragraphs, zoom, and font changes without losing the caret", async ({ page, server }) => {
  await openType(page, server);
  const m = await mapper(page);
  const c = m.toClient({ x: 2000, y: 4000 });
  await page.mouse.click(c.x, c.y);
  const editor = page.getByTestId("text-editor");
  await expect(editor).toHaveAttribute("data-canonical-width", "10000");
  const initial = (await editor.boundingBox())!;

  const wrappingText = "wrapping words across the wider paragraph ".repeat(28);
  await editor.fill(wrappingText);
  await expect.poll(async () => (await editor.boundingBox())!.height).toBeGreaterThan(initial.height * 3);
  const wrapped = (await editor.boundingBox())!;

  await editor.fill(`${wrappingText}\n${Array.from({ length: 5 }, (_, i) => `Paragraph line ${i + 1}.`).join("\n")}`);
  await expect.poll(async () => (await editor.boundingBox())!.height).toBeGreaterThan(wrapped.height);
  const paragraphs = (await editor.boundingBox())!;

  await pinch(page, 700, 420, 90, 160);
  await expect(page.getByTestId("text-editor")).toBeVisible();
  await expect.poll(async () => (await editor.boundingBox())!.height).toBeGreaterThan(paragraphs.height);
  const zoomed = (await editor.boundingBox())!;

  await page.getByTestId("text-size-up").click();
  await expect(editor).toHaveAttribute("data-font-size", "400");
  await expect(page.getByTestId("text-size-label")).toHaveText("118%");
  await expect.poll(async () => (await editor.boundingBox())!.height).toBeGreaterThan(zoomed.height);
  expect(await editor.evaluate((el) => document.activeElement === el)).toBe(true);

  await page.getByTestId("text-size-down").click();
  await expect(editor).toHaveAttribute("data-font-size", "340");
  await expect(page.getByTestId("text-size-label")).toHaveText("100%");
  expect(await editor.evaluate((el) => document.activeElement === el)).toBe(true);

  await page.getByTestId("done").click();
  await synced(page, server, 1);
  expect((await firstPageTexts(server))[0]!.font_size).toBe(340);
});

test("idle Text size is inherited by new blocks and resets only after successful Done", async ({ page, server, traffic }) => {
  await openType(page, server);
  await page.getByTestId("text-size-up").click();
  await expect(page.getByTestId("text-size-label")).toHaveText("118%");
  expect(writes(traffic)).toEqual([]);

  let m = await mapper(page);
  let c = m.toClient({ x: 1800, y: 3200 });
  await page.mouse.click(c.x, c.y);
  const editor = page.getByTestId("text-editor");
  await expect(editor).toHaveAttribute("data-font-size", "400");
  await editor.fill("first inherited");

  c = m.toClient({ x: 8500, y: 12500 });
  await page.mouse.click(c.x, c.y);
  await expect(editor).toHaveValue("");
  await expect(editor).toHaveAttribute("data-font-size", "400");
  await editor.fill("second inherited");
  await page.getByTestId("done").click();
  await synced(page, server, 2);
  expect((await firstPageTexts(server)).map((t) => t.font_size)).toEqual([400, 400]);
  expect(await opTypes(server)).toEqual(["add_text", "add_text"]);

  await enterType(page);
  await expect(page.getByTestId("text-size-label")).toHaveText("100%");
  m = await mapper(page);
  c = m.toClient({ x: 5000, y: 18000 });
  await page.mouse.click(c.x, c.y);
  await expect(editor).toHaveAttribute("data-font-size", "340");
  await page.getByTestId("done").click();
  await page.waitForTimeout(400);
  expect(await opTypes(server)).toEqual(["add_text", "add_text"]);
});

test("editor height stops at the paper bottom and scrolls internally", async ({ page, server }) => {
  await openType(page, server);
  const m = await mapper(page);
  const c = m.toClient({ x: 2000, y: 20_000 });
  await page.mouse.click(c.x, c.y);
  const editor = page.getByTestId("text-editor");
  await editor.fill(Array.from({ length: 30 }, (_, i) => `bottom line ${i + 1}`).join("\n"));
  await expect(editor).toHaveCSS("overflow-y", "auto");
  const metrics = await editor.evaluate((el) => ({ height: el.getBoundingClientRect().height, scrollHeight: el.scrollHeight, max: Number(el.dataset.maxPaperHeight) }));
  expect(metrics.height).toBeCloseTo(metrics.max, 0);
  expect(metrics.scrollHeight).toBeGreaterThan(metrics.height);

  const scroller = (await page.getByTestId("page-scroller").boundingBox())!;
  const { rect } = await canvasInfo(page);
  const box = (await editor.boundingBox())!;
  expect(box.y + box.height).toBeLessThanOrEqual(scroller.y + rect[1] + rect[3] + 1);
  await page.getByTestId("done").click();
  await synced(page, server, 1);
  expect((await firstPageTexts(server))[0]).not.toHaveProperty("height");
});

test("type-mode touch tap places text; touch pan does not", async ({ page, server, traffic }) => {
  await openType(page, server, { width: 390, height: 844 });

  // Tap with negligible movement → editor.
  const tapX = 180;
  const tapY = 320;
  await touch(page, "pointerdown", 1, tapX, tapY);
  await touch(page, "pointerup", 1, tapX, tapY);
  await expect(page.getByTestId("text-editor")).toBeVisible();
  await page.getByTestId("done").click();
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  expect(writes(traffic)).toEqual([]); // empty draft abandoned

  await enterType(page);
  // Zoom in so the page can actually pan (Fit often has nowhere to scroll).
  await page.getByTestId("zoom-in").click();
  await page.getByTestId("zoom-in").click();
  const scroller = page.getByTestId("page-scroller");
  const s0 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  await touch(page, "pointerdown", 2, 200, 400);
  await touch(page, "pointermove", 2, 140, 280);
  await touch(page, "pointerup", 2, 140, 280);
  const s1 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  expect(Math.abs(s1[0]! - s0[0]!) + Math.abs(s1[1]! - s0[1]!)).toBeGreaterThan(20);
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  expect(writes(traffic)).toEqual([]);
});

test("type-mode pinch does not mint text", async ({ page, server, traffic }) => {
  await openType(page, server, { width: 390, height: 844 });
  await pinch(page, 200, 300, 80, 180);
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  expect(writes(traffic)).toEqual([]);
  // Zoom should have changed (pinch out).
  await expect.poll(async () => (await canvasInfo(page)).zoom).toBeGreaterThan(1);
});

test("a press that starts off the paper cannot place text or mint an operation", async ({ page, server, traffic }) => {
  await openType(page, server);
  const scroller = (await page.getByTestId("page-scroller").boundingBox())!;
  const { rect, viewport } = await canvasInfo(page);
  const candidates = [
    { room: rect[0], x: rect[0] / 2, y: viewport[1] / 2 },
    { room: viewport[0] - rect[0] - rect[2], x: rect[0] + rect[2] + (viewport[0] - rect[0] - rect[2]) / 2, y: viewport[1] / 2 },
    { room: rect[1], x: viewport[0] / 2, y: rect[1] / 2 },
    { room: viewport[1] - rect[1] - rect[3], x: viewport[0] / 2, y: rect[1] + rect[3] + (viewport[1] - rect[1] - rect[3]) / 2 },
  ];
  const desk = candidates.sort((a, b) => b.room - a.room)[0]!;
  expect(desk.room).toBeGreaterThan(2);
  const inside = await mapper(page).then((m) => m.toClient({ x: 8000, y: 10000 }));

  // Press intent is OutsidePaper even though release lands on the sheet.
  await page.mouse.move(scroller.x + desk.x, scroller.y + desk.y);
  await page.mouse.down();
  await page.mouse.move(inside.x, inside.y);
  await page.mouse.up();

  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  expect(writes(traffic)).toEqual([]);
  expect(await opTypes(server)).toEqual([]);
});

test("type-mode pan over existing text does not open the editor", async ({ page, server }) => {
  await openType(page, server, { width: 390, height: 844 });
  const m = await mapper(page);
  const c = m.toClient({ x: 2000, y: 4000 });
  // Place via mouse (desktop path unchanged), commit content.
  await page.mouse.click(c.x, c.y);
  await page.getByTestId("text-editor").fill("keep");
  await page.getByTestId("done").click();
  await synced(page, server, 1);
  await expectPageText(page, "keep");

  await enterType(page);
  await page.getByTestId("zoom-in").click();
  await page.getByTestId("zoom-in").click();
  const text = page.getByTestId("page-text");
  const tb = (await text.boundingBox())!;
  const sb = (await page.getByTestId("page-scroller").boundingBox())!;
  const localX = tb.x - sb.x + tb.width / 2;
  const localY = tb.y - sb.y + tb.height / 2;
  await touch(page, "pointerdown", 3, localX, localY);
  await touch(page, "pointermove", 3, localX - 80, localY - 60);
  await touch(page, "pointerup", 3, localX - 80, localY - 60);
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
});

test("click existing text in read mode enters type and opens the editor", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "from read");
  await synced(page, server, 1);
  await expect(page.getByTestId("type")).toBeVisible();

  await page.getByTestId("page-text").click();
  const editor = page.getByTestId("text-editor");
  await expect(editor).toBeVisible();
  await expect(editor).toHaveValue("from read");
  await expect(page.getByTestId("done")).toBeVisible();
});

test("click existing text edits content with one UpdateText", async ({ page, server, traffic }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "original");
  await synced(page, server, 1);
  const before = (await firstPageTexts(server))[0]!;
  const postsBefore = opPosts(traffic).length;

  await enterType(page);
  await page.getByTestId("page-text").click();
  const editor = page.getByTestId("text-editor");
  await expect(editor).toBeVisible();
  await expect(editor).toHaveValue("original");
  await editor.fill("original edited");
  await page.getByTestId("done").click();
  await synced(page, server, 2);

  expect(await opTypes(server)).toEqual(["add_text", "update_text"]);
  expect(opPosts(traffic).length).toBe(postsBefore + 1);
  const upd = await textOp(server, 1);
  expect(upd.text.text_id).toBe(before.text_id);
  expect(upd.text.content).toBe("original edited");
  expect({ x: upd.text.x, y: upd.text.y, width: upd.text.width, font_size: upd.text.font_size, face: upd.text.face }).toEqual({
    x: before.x,
    y: before.y,
    width: before.width,
    font_size: before.font_size,
    face: before.face,
  });

  await page.reload();
  await expectPageText(page, "original edited");
});

test("multiple font changes on existing text persist as one whole-object UpdateText", async ({ page, server, traffic }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "resize once");
  await synced(page, server, 1);
  const postsBefore = opPosts(traffic).length;

  await enterType(page);
  await page.getByTestId("page-text").click();
  const editor = page.getByTestId("text-editor");
  await page.getByTestId("text-size-up").click();
  await page.getByTestId("text-size-up").click();
  await expect(editor).toHaveAttribute("data-font-size", "480");
  await expect(page.getByTestId("text-size-label")).toHaveText("141%");
  expect(opPosts(traffic).length).toBe(postsBefore);
  await expect(editor).toHaveValue("resize once");

  await page.getByTestId("done").click();
  await synced(page, server, 2);
  expect(opPosts(traffic).length).toBe(postsBefore + 1);
  expect(await opTypes(server)).toEqual(["add_text", "update_text"]);
  const stored = (await firstPageTexts(server))[0]!;
  expect(stored.font_size).toBe(480);
  expect(stored.width).toBe(10_000);
});

test("legacy width and off-grid font size stay exact until an explicit size step", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "off grid");
  await synced(page, server, 1);
  await postTextUpdate(server, { x: 2000.5, y: 4000.5, width: 4321.5, font_size: 375.5 });
  const text = page.getByTestId("page-text");
  await expect(text).toHaveAttribute("data-canonical-width", "4321.5");
  await expect(text).toHaveAttribute("data-font-size", "375.5");

  // View activation seeds the exact stored value; opening and Done do not snap.
  await text.click();
  const editor = page.getByTestId("text-editor");
  await expect(editor).toHaveAttribute("data-canonical-width", "4321.5");
  await expect(editor).toHaveAttribute("data-font-size", "375.5");
  await expect(page.getByTestId("text-size-label")).toHaveText("110%");
  await page.getByTestId("done").click();
  await page.waitForTimeout(500);
  expect(await opTypes(server)).toEqual(["add_text", "update_text"]);
  expect((await firstPageTexts(server))[0]!.font_size).toBe(375.5);

  await page.getByTestId("page-text").click();
  await page.getByTestId("text-size-up").click();
  await expect(editor).toHaveAttribute("data-font-size", "400");
  await page.getByTestId("done").click();
  await synced(page, server, 3);
  const stored = (await firstPageTexts(server))[0]!;
  expect(stored.font_size).toBe(400);
  expect(stored.width).toBe(4321.5);
});

test("switching text joins blur finalization and commits the completing draft once", async ({ page, server, traffic }) => {
  await openType(page, server);
  await page.getByTestId("text-size-up").click();
  await placeText(page, 2000, 4000, "first");
  await enterType(page);
  await page.getByTestId("text-size-up").click();
  await page.getByTestId("text-size-up").click();
  await placeText(page, 9000, 12000, "second");
  await synced(page, server, 2);
  const postsBefore = opPosts(traffic).length;

  await enterType(page);
  await page.getByTestId("page-text").filter({ hasText: "first" }).click();
  const editor = page.getByTestId("text-editor");
  await expect(page.getByTestId("text-size-label")).toHaveText("118%");
  await editor.fill("first edited");

  // pointerdown blurs the editor and pointerup requests the switch. Both join
  // the same in-flight finalization rather than emitting duplicate updates.
  await page.getByTestId("page-text").filter({ hasText: "second" }).click();
  await expect(editor).toHaveValue("second");
  await expect(editor).toHaveAttribute("data-font-size", "480");
  await expect(page.getByTestId("text-size-label")).toHaveText("141%");
  await synced(page, server, 3);
  expect(opPosts(traffic).length).toBe(postsBefore + 1);
  expect(await opTypes(server)).toEqual(["add_text", "add_text", "update_text"]);

  await page.getByTestId("done").click();
  await page.waitForTimeout(500);
  expect(await opTypes(server)).toEqual(["add_text", "add_text", "update_text"]);
  await expect(page.getByTestId("page-text")).toHaveCount(2);
});

test("click without movement does not reposition or emit", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "steady");
  await synced(page, server, 1);
  const before = (await firstPageTexts(server))[0]!;

  await enterType(page);
  await page.getByTestId("page-text").click();
  await expect(page.getByTestId("text-editor")).toBeVisible();
  await page.getByTestId("done").click();
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  await page.waitForTimeout(1000);
  expect(await opTypes(server)).toEqual(["add_text"]);
  const after = (await firstPageTexts(server))[0]!;
  expect({ x: after.x, y: after.y }).toEqual({ x: before.x, y: before.y });
});

test("drag moves text with exactly one UpdateText", async ({ page, server, traffic }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "drag me");
  await synced(page, server, 1);
  const before = (await firstPageTexts(server))[0]!;
  const postsBefore = opPosts(traffic).length;

  await enterType(page);
  const box = (await page.getByTestId("page-text").boundingBox())!;
  const sx = box.x + box.width / 2;
  const sy = box.y + box.height / 2;
  await page.mouse.move(sx, sy);
  await page.mouse.down();
  await page.mouse.move(sx + 90, sy + 70, { steps: 15 });
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  const moved = (await page.getByTestId("page-text").boundingBox())!;
  expect(Math.abs(moved.x - box.x) + Math.abs(moved.y - box.y)).toBeGreaterThan(20);
  await page.mouse.up();
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  await synced(page, server, 2);

  expect(opPosts(traffic).length).toBe(postsBefore + 1);
  expect(await opTypes(server)).toEqual(["add_text", "update_text"]);
  const upd = await textOp(server, 1);
  expect(upd.text.text_id).toBe(before.text_id);
  expect(upd.text.content).toBe("drag me");
  expect({ width: upd.text.width, font_size: upd.text.font_size, face: upd.text.face }).toEqual({
    width: before.width,
    font_size: before.font_size,
    face: before.face,
  });
  expect(upd.text.x).not.toBe(before.x);
  expect(upd.text.y).not.toBe(before.y);

  await page.reload();
  await expectPageText(page, "drag me");
  const kept = (await firstPageTexts(server))[0]!;
  expect({ x: kept.x, y: kept.y }).toEqual({ x: upd.text.x, y: upd.text.y });
});

test("drag beyond page edges clamps to valid geometry", async ({ page, server }) => {
  await openType(page, server, { width: 1600, height: 1200 });
  await placeText(page, 2000, 4000, "edge");
  await synced(page, server, 1);

  await enterType(page);
  const box = (await page.getByTestId("page-text").boundingBox())!;
  const sx = box.x + box.width / 2;
  const sy = box.y + box.height / 2;
  // Far enough past the page edge to prove clamping, but inside the viewport:
  // a release outside the window is a cancel, not a drop.
  await page.mouse.move(sx, sy);
  await page.mouse.down();
  await page.mouse.move(sx + 700, sy + 500, { steps: 20 });
  await page.mouse.up();
  await synced(page, server, 2);

  const after = (await firstPageTexts(server))[0]!;
  expect(after.x).toBeGreaterThanOrEqual(0);
  expect(after.y).toBeGreaterThanOrEqual(0);
  expect(after.x + after.width).toBeLessThanOrEqual(15624);
  expect(after.y).toBeLessThan(20832);
  expect(after.x + after.width).toBe(15624);
});

test("delete affordance removes existing text; new drafts show none", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "doomed");
  await synced(page, server, 1);

  await enterType(page);
  await page.getByTestId("page-text").click();
  await expect(page.getByTestId("text-editor")).toBeVisible();
  await page.getByTestId("text-delete").click();
  await synced(page, server, 2);
  expect(await opTypes(server)).toEqual(["add_text", "delete_text"]);
  await expect(page.getByTestId("page-text")).toHaveCount(0);
  await page.reload();
  await expect(page.getByTestId("page-text")).toHaveCount(0);
  expect(await firstPageTexts(server)).toHaveLength(0);
});

test("new drafts show no delete affordance; emptying deletes", async ({ page, server }) => {
  await openType(page, server);
  const m = await mapper(page);
  const c = m.toClient({ x: 6000, y: 6000 });
  await page.mouse.click(c.x, c.y);
  await expect(page.getByTestId("text-editor")).toBeVisible();
  await expect(page.getByTestId("text-delete")).toHaveCount(0);
  await page.getByTestId("done").click();
  await expect(page.getByTestId("text-editor")).toHaveCount(0);

  await enterType(page);
  await placeText(page, 2000, 4000, "temporary");
  await synced(page, server, 1);
  await enterType(page);
  await page.getByTestId("page-text").click();
  const editor = page.getByTestId("text-editor");
  await expect(editor).toBeVisible();
  await editor.fill("");
  await page.getByTestId("done").click();
  await synced(page, server, 2);
  expect(await opTypes(server)).toEqual(["add_text", "delete_text"]);
});

test("drag preserves a concurrent content edit", async ({ page, server, browser }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "content A");
  await synced(page, server, 1);
  const orig = (await firstPageTexts(server))[0]!;
  await enterType(page);

  const otherContext = await browser.newContext();
  const other = await otherContext.newPage();
  try {
    await other.setViewportSize({ width: 1280, height: 900 });
    await connect(other, server);
    await enterType(other);

    // Synthetic hold on the main tab: a real mouse held across two tabs shares
    // button state in some browsers, which would corrupt the other tab's click.
    await textTouch(page, "pointerdown", 43, 0, 0);

    const m = await mapper(other);
    const c = m.toClient({ x: 2000, y: 4000 });
    await other.mouse.click(c.x, c.y);
    const editor = other.getByTestId("text-editor");
    await expect(editor).toBeVisible();
    await editor.fill("content B");
    await other.getByTestId("done").click();
    await synced(other, server, 2);

    await expect.poll(async () => page.getByTestId("page-text").textContent()).toBe("content B");

    for (let i = 1; i <= 10; i++) await textTouch(page, "pointermove", 43, i * 8, i * 6);
    await textTouch(page, "pointerup", 43, 80, 60);
    await synced(page, server, 3);

    const after = (await firstPageTexts(server))[0]!;
    expect(after.text_id).toBe(orig.text_id);
    expect(after.content).toBe("content B");
    expect(after.x).not.toBe(orig.x);
    expect(after.y).not.toBe(orig.y);
    expect(await opTypes(server)).toEqual(["add_text", "update_text", "update_text"]);
  } finally {
    await otherContext.close();
  }
});

test("remote delete before drop cancels the drag without an update", async ({ page, server, browser, traffic }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "vanishing");
  await synced(page, server, 1);
  const postsBefore = opPosts(traffic).length;
  await enterType(page);

  const otherContext = await browser.newContext();
  const other = await otherContext.newPage();
  try {
    await other.setViewportSize({ width: 1280, height: 900 });
    await connect(other, server);
    await enterType(other);

    // Synthetic hold on the main tab: a real mouse held across two tabs shares
    // button state in some browsers, which would corrupt the other tab's click.
    const box = (await page.getByTestId("page-text").boundingBox())!;
    const sb = (await page.getByTestId("page-scroller").boundingBox())!;
    const lx = box.x - sb.x + box.width / 2;
    const ly = box.y - sb.y + box.height / 2;
    await textTouch(page, "pointerdown", 43, 0, 0);

    const m = await mapper(other);
    const c = m.toClient({ x: 2000, y: 4000 });
    await other.mouse.click(c.x, c.y);
    const editor = other.getByTestId("text-editor");
    await expect(editor).toBeVisible();
    await editor.fill("");
    await other.getByTestId("done").click();
    await synced(other, server, 2);

    await expect(page.getByTestId("page-text")).toHaveCount(0);

    // The text div is gone; release the held pointer over the scroller instead.
    await touch(page, "pointermove", 43, lx + 80, ly + 60);
    await touch(page, "pointerup", 43, lx + 80, ly + 60);
    // Strict: the cancelled release must create no editor, no draft, no UpdateText, no conflict.
    await page.waitForTimeout(1000);
    await expect(page.getByTestId("text-editor")).toHaveCount(0);
    await expect(page.getByTestId("page-text")).toHaveCount(0);
    expect(opPosts(traffic).length).toBe(postsBefore);
    expect(await opTypes(server)).toEqual(["add_text", "delete_text"]);
    await expect(page.getByTestId("sync-status")).not.toHaveAttribute("data-phase", "conflict");
    await expect(page.getByTestId("sync-conflict")).toHaveCount(0);
  } finally {
    await otherContext.close();
  }
});

test("remote delete during edit preserves unsaved typing", async ({ page, server, browser, traffic }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "hello");
  await enterType(page);
  await placeText(page, 9000, 14000, "other text");
  await synced(page, server, 2);
  const postsBefore = opPosts(traffic).length;

  await enterType(page);
  await page.getByTestId("page-text").filter({ hasText: "hello" }).click();
  const editor = page.getByTestId("text-editor");
  await expect(editor).toBeVisible();
  await editor.fill("hello edited but unsaved");
  await page.getByTestId("text-size-up").click();
  await expect(page.getByTestId("text-size-label")).toHaveText("118%");
  await expect(editor).toHaveAttribute("data-font-size", "400");

  const otherContext = await browser.newContext();
  const other = await otherContext.newPage();
  try {
    await other.setViewportSize({ width: 1280, height: 900 });
    await connect(other, server);
    await enterType(other);
    await other.getByTestId("page-text").filter({ hasText: "hello" }).click();
    const otherEditor = other.getByTestId("text-editor");
    await expect(otherEditor).toBeVisible();
    await otherEditor.fill("");
    await other.getByTestId("done").click();
    await synced(other, server, 3);

    // The editor stays open with the unsaved typing intact.
    await expect(page.getByTestId("save-error")).toContainText("deleted elsewhere");
    await expect(editor).toHaveValue("hello edited but unsaved");

    // Switching to another block is blocked and preserves the same editor.
    await page.getByTestId("page-text").filter({ hasText: "other text" }).click();
    await expect(editor).toHaveValue("hello edited but unsaved");

    // Blank placement is blocked too: the active draft is not replaced.
    const mainMapper = await mapper(page);
    const blank = mainMapper.toClient({ x: 12000, y: 18000 });
    await page.mouse.click(blank.x, blank.y);
    await expect(editor).toHaveValue("hello edited but unsaved");

    // Done cannot leave Text mode while finalization is blocked.
    await page.getByTestId("done").click();
    await page.waitForTimeout(1000);
    await expect(page.getByTestId("done")).toBeVisible();
    await expect(page.getByTestId("type")).toHaveCount(0);
    await expect(page.getByTestId("text-size-label")).toHaveText("118%");
    await expect(editor).toBeVisible();
    await expect(editor).toHaveValue("hello edited but unsaved");

    // Page and sidebar navigation use that same transition gate.
    await page.getByTestId("next").click();
    await expect(page.getByTestId("page-label")).toHaveText("1 of 3");
    const notebooks = page.getByTestId("notebook-item");
    await notebooks.nth(1).click();
    await expect(notebooks.nth(0)).toHaveAttribute("aria-current", "true");
    await expect(editor).toHaveValue("hello edited but unsaved");

    // Structural actions cannot invalidate the bound page either.
    await page.getByTestId("insert-page").click();
    await expect(page.getByTestId("page-label")).toHaveText("1 of 3");
    await page.getByTestId("delete-page").click();
    await page.getByTestId("confirm-delete-page").click();
    await expect(page.getByTestId("page-label")).toHaveText("1 of 3");
    await expect(editor).toHaveValue("hello edited but unsaved");

    // Link priority remains, but its navigation cannot bypass finalization.
    await postPageLink(server);
    await expect(page.getByTestId("page-link")).toBeVisible();
    await page.getByTestId("page-link").click();
    await expect(page.getByTestId("page-label")).toHaveText("1 of 3");
    await expect(editor).toHaveValue("hello edited but unsaved");

    expect(opPosts(traffic).length).toBe(postsBefore);
    expect(await opTypes(server)).toEqual(["add_text", "add_text", "delete_text", "add_page_link"]);
    await expect(page.getByTestId("sync-conflict")).toHaveCount(0);

    // Explicit delete discards the preserved typing with no operation.
    await page.getByTestId("text-delete").click();
    await expect(page.getByTestId("text-editor")).toHaveCount(0);
    await page.waitForTimeout(500);
    expect(opPosts(traffic).length).toBe(postsBefore);
    expect(await opTypes(server)).toEqual(["add_text", "add_text", "delete_text", "add_page_link"]);

    // Once Done can succeed, the next Text session returns to 100%.
    await page.getByTestId("done").click();
    await enterType(page);
    await expect(page.getByTestId("text-size-label")).toHaveText("100%");
  } finally {
    await otherContext.close();
  }
});

test("mode switch during drag cancels; the next grab still works", async ({ page, server, traffic }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "held");
  await synced(page, server, 1);
  const before = (await firstPageTexts(server))[0]!;
  const postsBefore = opPosts(traffic).length;

  await enterType(page);
  // Begin a drag with a synthetic touch pointer and move beyond slop.
  await textTouch(page, "pointerdown", 43, 0, 0);
  for (let i = 1; i <= 5; i++) await textTouch(page, "pointermove", 43, i * 20, i * 10);
  await expect(page.getByTestId("page-text")).toHaveClass(/dragging/);

  // Leave Type mode before releasing, then release: no move op, no draft, no editor.
  await page.getByTestId("done").click();
  await expect(page.getByTestId("type")).toBeVisible();
  await textTouch(page, "pointerup", 43, 100, 50);
  await page.waitForTimeout(1000);
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  expect(opPosts(traffic).length).toBe(postsBefore);
  expect(await opTypes(server)).toEqual(["add_text"]);
  const kept = (await firstPageTexts(server))[0]!;
  expect({ x: kept.x, y: kept.y }).toEqual({ x: before.x, y: before.y });

  // Re-enter Type mode: clicking the text edits normally.
  await enterType(page);
  await page.getByTestId("page-text").click();
  const editor = page.getByTestId("text-editor");
  await expect(editor).toBeVisible();
  await expect(editor).toHaveValue("held");
  await page.getByTestId("done").click();
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  expect(await opTypes(server)).toEqual(["add_text"]);

  // A reused pointer ID works too (its stale suppression died with the fresh press).
  await enterType(page);
  await textTouch(page, "pointerdown", 43, 0, 0);
  await textTouch(page, "pointerup", 43, 0, 0);
  await expect(page.getByTestId("text-editor")).toBeVisible();
  await page.getByTestId("done").click();

  // And a drag commits exactly one UpdateText.
  await enterType(page);
  const box = (await page.getByTestId("page-text").boundingBox())!;
  const sx = box.x + box.width / 2;
  const sy = box.y + box.height / 2;
  await page.mouse.move(sx, sy);
  await page.mouse.down();
  await page.mouse.move(sx + 90, sy + 70, { steps: 10 });
  await page.mouse.up();
  await synced(page, server, 2);
  expect(await opTypes(server)).toEqual(["add_text", "update_text"]);
  const moved = (await firstPageTexts(server))[0]!;
  expect({ x: moved.x, y: moved.y }).not.toEqual({ x: before.x, y: before.y });
});

test("page switch during drag cancels without committing", async ({ page, server, traffic }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "stays");
  await synced(page, server, 1);
  const before = (await firstPageTexts(server))[0]!;
  const postsBefore = opPosts(traffic).length;

  await enterType(page);
  const box = (await page.getByTestId("page-text").boundingBox())!;
  const sx = box.x + box.width / 2;
  const sy = box.y + box.height / 2;
  await page.mouse.move(sx, sy);
  await page.mouse.down();
  await page.mouse.move(sx + 90, sy + 70, { steps: 10 });
  await expect(page.getByTestId("page-text")).toHaveClass(/dragging/);

  await page.keyboard.press("ArrowRight");
  await expect(page.getByTestId("page-label")).toHaveText("2 of 3");
  await page.waitForTimeout(200);

  // Releasing the old pointer creates nothing on the new page.
  await page.mouse.up();
  await page.waitForTimeout(1000);
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  await expect(page.getByTestId("page-text")).toHaveCount(0);
  expect(opPosts(traffic).length).toBe(postsBefore);
  expect(await opTypes(server)).toEqual(["add_text"]);
  await expect(page.getByTestId("sync-conflict")).toHaveCount(0);

  await page.getByTestId("prev").click();
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3");
  await expectPageText(page, "stays");
  const kept = (await firstPageTexts(server))[0]!;
  expect({ x: kept.x, y: kept.y }).toEqual({ x: before.x, y: before.y });
});

test("pinch during an armed grab cancels without committing", async ({ page, server, traffic }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "pinched");
  await synced(page, server, 1);
  const before = (await firstPageTexts(server))[0]!;
  const postsBefore = opPosts(traffic).length;

  await enterType(page);
  // First finger presses the text and starts dragging.
  await textTouch(page, "pointerdown", 1, 0, 0);
  await textTouch(page, "pointermove", 1, 30, 0);
  await textTouch(page, "pointermove", 1, 60, 0);
  await expect(page.getByTestId("page-text")).toHaveClass(/dragging/);

  // A second finger takes ownership: pinch wins, the grab is cancelled.
  const sb = (await page.getByTestId("page-scroller").boundingBox())!;
  const tb = (await page.getByTestId("page-text").boundingBox())!;
  const cx = tb.x - sb.x + tb.width / 2;
  const cy = tb.y - sb.y + tb.height / 2;
  await touch(page, "pointerdown", 2, cx + 40, cy);
  for (let i = 1; i <= 6; i++) {
    await touch(page, "pointermove", 1, cx - 40 - i * 10, cy);
    await touch(page, "pointermove", 2, cx + 40 + i * 10, cy);
  }
  await touch(page, "pointerup", 1, cx - 100, cy);
  await touch(page, "pointerup", 2, cx + 100, cy);

  await page.waitForTimeout(1000);
  await expect.poll(async () => (await canvasInfo(page)).zoom).toBeGreaterThan(1);
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  expect(opPosts(traffic).length).toBe(postsBefore);
  expect(await opTypes(server)).toEqual(["add_text"]);
  const kept = (await firstPageTexts(server))[0]!;
  expect({ x: kept.x, y: kept.y }).toEqual({ x: before.x, y: before.y });
});

/** Post one page link over the first page's text at (3000, 5000) via the harness. */
async function postPageLink(server: Parameters<typeof connect>[1]) {
  const snap = await server.snapshot();
  const notebookId = snap.snapshot.notebooks[0]!.notebook_id;
  const pages = snap.snapshot.notebooks[0]!.pages;
  // Fractional source so JSON.stringify emits canonical float spellings (serde would write 3000 as 3000.0).
  const link = {
    link_id: randomUUID(),
    source_notebook_id: notebookId,
    source_page_id: pages[0]!.page_id,
    source_x: 3000.5,
    source_y: 5000.5,
    target_notebook_id: notebookId,
    target_page_id: pages[1]!.page_id,
  };
  const env = JSON.stringify({
    v: 1,
    operation_id: randomUUID(),
    library_id: manifest.library_id,
    replica_id: randomUUID(),
    client_seq: 1,
    operation: { type: "add_page_link", notebook_id: notebookId, page_id: pages[0]!.page_id, link },
  });
  const posted = await server.api("POST", `/v1/libraries/${manifest.library_id}/operations`, `{"envelopes":[${env}]}`);
  expect(posted.status).toBe(200);
}

test("link icon keeps priority over underlying text", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 3000, 5000, "linked");
  await synced(page, server, 1);
  await postPageLink(server);
  await expect(page.getByTestId("page-link")).toBeVisible();

  // View: the link wins over the text beneath it.
  const m = await mapper(page);
  const c = m.toClient({ x: 3000, y: 5000 });
  await page.mouse.click(c.x, c.y);
  await expect(page.getByTestId("page-label")).toHaveText("2 of 3");
  await expect(page.getByTestId("text-editor")).toHaveCount(0);

  await page.getByTestId("prev").click();
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3");

  // Text: the link still wins and navigation keeps Text mode active.
  await enterType(page);
  await page.mouse.click(c.x, c.y);
  await expect(page.getByTestId("page-label")).toHaveText("2 of 3");
  await expect(page.getByTestId("done")).toBeVisible();
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  await page.getByTestId("prev").click();
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3");

  const headBefore = await server.head();
  const before = (await firstPageTexts(server))[0]!;
  const linkBox = (await page.getByTestId("page-link").boundingBox())!;
  const lx = linkBox.x + linkBox.width / 2;
  const ly = linkBox.y + linkBox.height / 2;
  await page.mouse.move(lx, ly);
  await page.mouse.down();
  await page.mouse.move(lx + 70, ly + 60, { steps: 8 });
  await page.mouse.up();
  await page.waitForTimeout(800);
  expect(await server.head()).toBe(headBefore);
  expect(await opTypes(server)).toEqual(["add_text", "add_page_link"]);
  const kept = (await firstPageTexts(server))[0]!;
  expect({ x: kept.x, y: kept.y }).toEqual({ x: before.x, y: before.y });
});

test("Ink owns link and text pixels instead of navigating or entering Text", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 3000, 5000, "ink target");
  await synced(page, server, 1);
  await postPageLink(server);
  await expect(page.getByTestId("page-link")).toBeVisible();

  await enterWrite(page);
  await expect(page.getByTestId("page-link")).toHaveCSS("pointer-events", "none");
  await expect(page.getByTestId("page-text")).toHaveCSS("pointer-events", "none");
  const linkBox = (await page.getByTestId("page-link").boundingBox())!;
  const owner = await page.evaluate(
    ({ x, y }) => {
      const hit = document.elementFromPoint(x, y);
      return {
        link: Boolean(hit?.closest("[data-link-id]")),
        text: Boolean(hit?.closest("[data-text-id]")),
        scroller: Boolean(hit?.closest('[data-testid="page-scroller"]')),
      };
    },
    { x: linkBox.x + linkBox.width / 2, y: linkBox.y + linkBox.height / 2 },
  );
  expect(owner).toEqual({ link: false, text: false, scroller: true });

  // Synthetic stylus input starts on the shared link/text pixel. The ink path
  // receives it, authors one stroke, and never activates either overlay.
  await drawPen(page, [
    { x: 3000.5, y: 5000.5, pressure: 0.5 },
    { x: 3600, y: 5400, pressure: 0.7 },
  ]);
  await synced(page, server, 3);
  expect(await opTypes(server)).toEqual(["add_text", "add_page_link", "add_stroke"]);
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3");
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  await expect(page.getByTestId("pen")).toBeVisible();
});

test("touch drag inside the text editor does not pan or zoom", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "editing");
  await synced(page, server, 1);
  await enterType(page);
  // Zoom in so the page can actually pan (a no-scroll assertion at Fit would be vacuous).
  await page.getByTestId("zoom-in").click();
  await page.getByTestId("zoom-in").click();
  await page.getByTestId("page-text").click();
  const editor = page.getByTestId("text-editor");
  await expect(editor).toBeVisible();
  await editor.fill("editing changed");
  const scroller = page.getByTestId("page-scroller");
  const s0 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  const z0 = (await canvasInfo(page)).zoom;

  await touchOn(page, "text-editor", "pointerdown", 7, 0, 0);
  for (let i = 1; i <= 5; i++) await touchOn(page, "text-editor", "pointermove", 7, i * 20, i * 10);
  await touchOn(page, "text-editor", "pointerup", 7, 100, 50);
  await page.waitForTimeout(500);

  const s1 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  expect(s1).toEqual(s0);
  expect((await canvasInfo(page)).zoom).toBe(z0);
  await expect(page.getByTestId("text-editor")).toBeVisible();
  await expect(page.getByTestId("text-editor")).toHaveValue("editing changed");
  expect(await opTypes(server)).toEqual(["add_text"]);
});

test("touch drag from the delete control does not pan or zoom; tap still deletes", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "doomed touch");
  await synced(page, server, 1);
  await enterType(page);
  await page.getByTestId("zoom-in").click();
  await page.getByTestId("zoom-in").click();
  await page.getByTestId("page-text").click();
  await expect(page.getByTestId("text-editor")).toBeVisible();
  const scroller = page.getByTestId("page-scroller");
  const s0 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  const z0 = (await canvasInfo(page)).zoom;

  // A cancel terminal: moves must not pan, and no tap may fire from the gesture.
  await touchOn(page, "text-delete", "pointerdown", 8, 0, 0);
  for (let i = 1; i <= 5; i++) await touchOn(page, "text-delete", "pointermove", 8, i * 20, i * 10);
  await touchOn(page, "text-delete", "pointercancel", 8, 100, 50);
  await page.waitForTimeout(500);

  const s1 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  expect(s1).toEqual(s0);
  expect((await canvasInfo(page)).zoom).toBe(z0);
  await expect(page.getByTestId("text-editor")).toBeVisible();
  expect(await opTypes(server)).toEqual(["add_text"]);

  // Synthetic tap (single modality: mixing synthetic touch with a real mouse click
  // trips a WebKit focus quirk). Real touch taps are covered by the test below.
  await touchOn(page, "text-delete", "pointerdown", 10, 0, 0);
  await touchOn(page, "text-delete", "pointerup", 10, 0, 0);
  await touchOn(page, "text-delete", "click", 10, 0, 0);
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  await synced(page, server, 2);
  expect(await opTypes(server)).toEqual(["add_text", "delete_text"]);
});

test("real touch tap edits and deletes text", async ({ browser, server }) => {
  const ctx = await browser.newContext({ hasTouch: true });
  const page = await ctx.newPage();
  try {
    await server.seedFixture();
    await page.setViewportSize({ width: 1280, height: 900 });
    await connect(page, server);
    await enterType(page);
    const m = await mapper(page);
    const c = m.toClient({ x: 2000, y: 4000 });
    await page.touchscreen.tap(c.x, c.y);
    await expect(page.getByTestId("text-editor")).toBeVisible();
    await page.getByTestId("text-editor").fill("tapped");
    await page.getByTestId("done").click();
    await expect(page.getByTestId("page-text")).toHaveText("tapped");
    await enterType(page);
    const t = page.getByTestId("page-text");
    const tb = (await t.boundingBox())!;
    await page.touchscreen.tap(tb.x + tb.width / 2, tb.y + tb.height / 2);
    await expect(page.getByTestId("text-editor")).toBeVisible();
    const box = (await page.getByTestId("text-delete").boundingBox())!;
    await page.touchscreen.tap(box.x + box.width / 2, box.y + box.height / 2);
    await expect(page.getByTestId("text-editor")).toHaveCount(0);
    await synced(page, server, 2);
    expect(await opTypes(server)).toEqual(["add_text", "delete_text"]);
  } finally {
    await ctx.close();
  }
});

test("touch drag from a link icon pans neither page nor text", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 3000, 5000, "linked");
  await synced(page, server, 1);
  await enterType(page);
  await postPageLink(server);
  await expect(page.getByTestId("page-link")).toBeVisible();
  await page.getByTestId("zoom-in").click();
  await page.getByTestId("zoom-in").click();
  const before = (await firstPageTexts(server))[0]!;
  const scroller = page.getByTestId("page-scroller");
  const s0 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  const z0 = (await canvasInfo(page)).zoom;

  await touchOn(page, "page-link", "pointerdown", 9, 0, 0);
  for (let i = 1; i <= 5; i++) await touchOn(page, "page-link", "pointermove", 9, i * 20, i * 10);
  await touchOn(page, "page-link", "pointercancel", 9, 100, 50);
  await page.waitForTimeout(500);

  const s1 = await scroller.evaluate((el) => [el.scrollLeft, el.scrollTop]);
  expect(s1).toEqual(s0);
  expect((await canvasInfo(page)).zoom).toBe(z0);
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3");
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  expect(await opTypes(server)).toEqual(["add_text", "add_page_link"]);
  const kept = (await firstPageTexts(server))[0]!;
  expect({ x: kept.x, y: kept.y }).toEqual({ x: before.x, y: before.y });
});

test("page switch during edit commits to the original page", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "bound");
  await synced(page, server, 1);

  await enterType(page);
  await page.getByTestId("page-text").click();
  const editor = page.getByTestId("text-editor");
  await expect(editor).toBeVisible();
  await editor.fill("bound edited");
  await page.getByTestId("next").click();
  await expect(page.getByTestId("page-label")).toHaveText("2 of 3");
  await expect(page.getByTestId("done")).toBeVisible();
  await expect(page.getByTestId("type")).toHaveCount(0);
  await synced(page, server, 2);

  const snap = await server.snapshot();
  expect(snap.snapshot.notebooks[0]!.pages[0]!.texts?.[0]?.content).toBe("bound edited");
  expect(snap.snapshot.notebooks[0]!.pages[1]!.texts ?? []).toHaveLength(0);
  await expect(page.getByTestId("save-error")).toHaveCount(0);
});

test("pointercancel during drag commits nothing", async ({ page, server }) => {
  await openType(page, server);
  await placeText(page, 2000, 4000, "cancel me");
  await synced(page, server, 1);
  const before = (await firstPageTexts(server))[0]!;

  await enterType(page);
  await page.getByTestId("page-text").evaluate((el) => {
    const r = el.getBoundingClientRect();
    const cx = r.left + r.width / 2;
    const cy = r.top + r.height / 2;
    const base = { pointerId: 42, pointerType: "touch", isPrimary: true, bubbles: true, cancelable: true, buttons: 1 };
    el.dispatchEvent(new PointerEvent("pointerdown", { ...base, clientX: cx, clientY: cy }));
    for (let i = 1; i <= 5; i++) {
      el.dispatchEvent(new PointerEvent("pointermove", { ...base, clientX: cx + i * 20, clientY: cy }));
    }
    el.dispatchEvent(new PointerEvent("pointercancel", { ...base, clientX: cx + 100, clientY: cy }));
    el.dispatchEvent(new PointerEvent("pointerup", { ...base, buttons: 0, clientX: cx + 100, clientY: cy }));
  });
  await page.waitForTimeout(800);
  expect(await opTypes(server)).toEqual(["add_text"]);
  await expect(page.getByTestId("text-editor")).toHaveCount(0);
  const kept = (await firstPageTexts(server))[0]!;
  expect({ x: kept.x, y: kept.y }).toEqual({ x: before.x, y: before.y });
});

test("phone viewport keeps type tools and drawer toggle reachable", async ({ page, server }) => {
  await openType(page, server, { width: 390, height: 844 });
  await expect(page.getByTestId("done")).toBeVisible();
  await expect(page.getByTestId("drawer-toggle")).toBeVisible();
  await page.getByTestId("drawer-toggle").click();
  await expect(page.getByTestId("notebooks")).toBeVisible();
  await expect(page.getByTestId("new-folder")).toBeVisible();
});
