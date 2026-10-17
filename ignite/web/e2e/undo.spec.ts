// Browser undo: DeleteStrokeLineage of the newest visible lineage root, including pending
// strokes and erase fragments; empty undo authors nothing.

import { canvasInfo, connect, expect, test } from "./harness";
import { browserMatchesServer, drawMouse, enterWrite, envelope, firstPageStrokes, opPosts, synced } from "./ink";

const LINE = [
  { x: 2000, y: 17000 },
  { x: 5500, y: 17000 },
  { x: 9000, y: 17000 },
];
const CUT = [
  { x: 5500, y: 16400 },
  { x: 5500, y: 17600 },
];

test("undo last stroke; empty undo posts nothing; second browser converges", async ({ page, server, traffic, browser }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const before = (await firstPageStrokes(server)).length;
  await enterWrite(page);
  await drawMouse(page, LINE);
  await expect(page.getByTestId("undo")).toBeEnabled();
  await synced(page, server, 1);
  expect((await firstPageStrokes(server)).length).toBe(before + 1);

  await page.getByTestId("undo").click();
  await synced(page, server, 2);
  expect((await firstPageStrokes(server)).length).toBe(before);
  const ops = (await server.operations()).map((o) => envelope(o.envelope));
  expect(ops.map((o) => o.operation.type)).toEqual(["add_stroke", "delete_stroke_lineage"]);
  expect(ops[1]!.operation.root_id).toBe(ops[0]!.operation.stroke.stroke_id);

  const otherContext = await browser.newContext();
  const other = await otherContext.newPage();
  await other.setViewportSize({ width: 1280, height: 900 });
  await connect(other, server);
  await browserMatchesServer(other, server);
  await otherContext.close();

  // A fresh page has no ink: undo is a no-op (the fixture page still has strokes).
  await page.getByTestId("insert-page").click();
  await expect(page.getByTestId("page-label")).toHaveText("2 of 4");
  await synced(page, server, 3);
  const n = opPosts(traffic).length;
  await expect(page.getByTestId("undo")).toBeDisabled();
  await page.getByTestId("undo").click({ force: true });
  await page.waitForTimeout(800);
  expect(opPosts(traffic)).toHaveLength(n);
});

test("offline AddStroke then DeleteStrokeLineage survive reconnect in client order", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await server.halt();
  await drawMouse(page, LINE);
  await expect(page.getByTestId("undo")).toBeEnabled();
  await page.getByTestId("undo").click();
  await expect(page.getByTestId("sync-status")).toHaveText("Offline · 2 pending", { timeout: 15_000 });
  await expect(page.getByTestId("page-canvas")).toHaveAttribute("data-pending", "2");
  const offlineCount = (await canvasInfo(page)).strokes;
  await server.resume();
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await synced(page, server, 2);
  const ops = (await server.operations()).map((o) => envelope(o.envelope));
  expect(ops.map((o) => [o.client_seq, o.operation.type])).toEqual([
    [1, "add_stroke"],
    [2, "delete_stroke_lineage"],
  ]);
  expect(ops[1]!.operation.root_id).toBe(ops[0]!.operation.stroke.stroke_id);
  expect((await canvasInfo(page)).strokes).toBe(offlineCount);
  await browserMatchesServer(page, server);
});

test("undo after erase sends the lineage root_id, not a fragment id", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await drawMouse(page, LINE);
  await synced(page, server, 1);
  await page.getByTestId("eraser").click();
  await drawMouse(page, CUT);
  await synced(page, server, 2);
  const afterErase = await firstPageStrokes(server);
  const add = envelope((await server.operations())[0]!.envelope);
  const root = add.operation.stroke.stroke_id;
  const fragments = afterErase.filter((s) => s.root_id === root);
  expect(fragments.length).toBeGreaterThan(1);
  expect(fragments.some((s) => s.stroke_id !== s.root_id)).toBe(true);

  await page.getByTestId("pen").click();
  await page.getByTestId("undo").click();
  await synced(page, server, 3);
  const undo = envelope((await server.operations())[2]!.envelope);
  expect(undo.operation.type).toBe("delete_stroke_lineage");
  expect(undo.operation.root_id).toBe(root);
  expect(undo.operation.root_id).not.toBe(fragments.at(-1)!.stroke_id);
  expect((await firstPageStrokes(server)).filter((s) => s.root_id === root)).toHaveLength(0);
});
