// S3B erase slice end-to-end: browser EraseRegion against the real Rust server. The server's
// reducer is the authority; the browser's local result must equal it once caught up.

import type { Page } from "@playwright/test";
import { canvasInfo, connect, expect, test, type SyncServer } from "./harness";
import { browserMatchesServer, drawMouse, enterWrite, envelope, firstPageStrokes, mapper, opPosts, synced, writes } from "./ink";

type Pt = { x: number; y: number };

async function useEraser(page: Page) {
  await page.getByTestId("eraser").click();
  await expect(page.getByTestId("eraser")).toHaveAttribute("aria-pressed", "true");
}

async function usePen(page: Page) {
  await page.getByTestId("pen").click();
  await expect(page.getByTestId("pen")).toHaveAttribute("aria-pressed", "true");
}

/** Synthetic pen input with the eraser end (buttons bit 5), optionally cancelled before up. */
async function penEraserEnd(page: Page, pts: Pt[], opts: { cancel?: boolean } = {}) {
  const m = await mapper(page);
  const c = pts.map(m.toClient);
  await page.getByTestId("page-scroller").evaluate(
    (el, { samples, cancel }) => {
      const ev = (type: string, s: { x: number; y: number }, buttons: number) =>
        el.dispatchEvent(
          new PointerEvent(type, {
            pointerId: 9,
            pointerType: "pen",
            isPrimary: true,
            bubbles: true,
            cancelable: true,
            clientX: s.x,
            clientY: s.y,
            pressure: 0.5,
            button: type === "pointerdown" ? 5 : -1,
            buttons,
          }),
        );
      ev("pointerdown", samples[0]!, 32);
      for (const s of samples.slice(1)) ev("pointermove", s, 32);
      if (cancel) ev("pointercancel", samples[samples.length - 1]!, 0);
      else ev("pointerup", samples[samples.length - 1]!, 0);
    },
    { samples: c, cancel: opts.cancel ?? false },
  );
}

/** The browser-authored op at `server_seq` (1-based position in the ledger). */
async function op(server: SyncServer, seq: number) {
  const ops = await server.operations();
  return envelope(ops[seq - 1]!.envelope) as unknown as {
    replica_id: string;
    client_seq: number;
    operation: { type: string; observed_roots: string[]; radius: number; path: { x: number; y: number; pressure?: number }[] };
  };
}

// A horizontal stroke low on the page (clear of the fixture's ink), and a vertical sweep through it.
const LINE: Pt[] = [
  { x: 2000, y: 17000 },
  { x: 5500, y: 17000 },
  { x: 9000, y: 17000 },
];
const CUT: Pt[] = [
  { x: 5500, y: 16400 },
  { x: 5500, y: 17600 },
];

async function myStrokeLineage(server: SyncServer, strokeRoot: string) {
  return (await firstPageStrokes(server)).filter((s) => s.root_id === strokeRoot);
}

test("erase: one EraseRegion per changing gesture; fragments equal the server's", async ({ page, server, traffic }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  await enterWrite(page);
  await drawMouse(page, LINE);
  await synced(page, server, 1);
  const root = envelope((await server.operations())[0]!.envelope).operation.stroke.stroke_id;

  // Selecting the eraser sends nothing; a sweep over empty paper authors nothing.
  await useEraser(page);
  await drawMouse(page, [
    { x: 12000, y: 19500 },
    { x: 14000, y: 19800 },
  ]);
  await page.waitForTimeout(1_500);
  expect(opPosts(traffic)).toHaveLength(1);

  // A real cut: exactly one more POST, an erase_region with the Kindle radius.
  await drawMouse(page, CUT);
  await synced(page, server, 2);
  expect(opPosts(traffic)).toHaveLength(2);
  expect(writes(traffic)).toHaveLength(2);
  const er = await op(server, 2);
  expect(er.operation.type).toBe("erase_region");
  expect(er.operation.radius).toBe(240);
  expect(er.client_seq).toBe(2);
  expect(er.operation.observed_roots).toContain(root);
  expect([...er.operation.observed_roots].sort()).toEqual(er.operation.observed_roots);
  expect(er.operation.path.every((p) => p.pressure === undefined)).toBe(true);

  // The reducer split our stroke in two; the browser shows exactly the server's page.
  const pieces = await myStrokeLineage(server, root);
  expect(pieces).toHaveLength(2);
  expect(pieces.every((p) => p.stroke_id !== root)).toBe(true);
  await browserMatchesServer(page, server);
  await expect(page.getByTestId("page-canvas")).toHaveAttribute("data-preview", "false");
  expect(traffic.consoleErrors).toEqual([]);
});

test("the pen's eraser end erases while the Pen tool is selected", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await drawMouse(page, LINE);
  await synced(page, server, 1);
  await expect(page.getByTestId("pen")).toHaveAttribute("aria-pressed", "true");
  await penEraserEnd(page, CUT);
  await synced(page, server, 2);
  expect((await op(server, 2)).operation.type).toBe("erase_region");
  await browserMatchesServer(page, server);
});

test("cancelling an erase mid-gesture authors nothing and restores the page", async ({ page, server, traffic }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await drawMouse(page, LINE);
  await synced(page, server, 1);
  const before = (await canvasInfo(page)).strokes;
  await penEraserEnd(page, CUT, { cancel: true });
  await page.waitForTimeout(1_500);
  expect(opPosts(traffic)).toHaveLength(1);
  expect(await server.head()).toBe(1);
  await expect(page.getByTestId("page-canvas")).toHaveAttribute("data-preview", "false");
  expect((await canvasInfo(page)).strokes).toBe(before);
});

test("erasing a Kindle stroke: observed-roots + fragments converge with the server", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await useEraser(page);
  // Sweep across the middle of the fixture's first stroke on this page.
  const target = (await firstPageStrokes(server))[0]!;
  const mid = target.points[Math.floor(target.points.length / 2)]!;
  await drawMouse(page, [
    { x: mid.x - 300, y: mid.y - 300 },
    { x: mid.x + 300, y: mid.y + 300 },
  ]);
  await synced(page, server, 1);
  const er = await op(server, 1);
  expect(er.operation.type).toBe("erase_region");
  expect(er.operation.observed_roots).toContain(target.root_id);
  expect((await firstPageStrokes(server)).some((s) => s.stroke_id === target.stroke_id)).toBe(false);
  await browserMatchesServer(page, server);
});

test("offline: draw then erase before sync; the local result holds and both sync in order", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await server.halt();
  await drawMouse(page, LINE);
  await useEraser(page);
  await drawMouse(page, CUT);
  await expect(page.getByTestId("sync-status")).toHaveText("Offline · 2 pending", { timeout: 15_000 });
  // Layer 2 already shows the reducer's result: the pending stroke, cut in two.
  await expect(page.getByTestId("page-canvas")).toHaveAttribute("data-pending", "2");
  await expect(page.getByTestId("page-canvas")).toHaveAttribute("data-preview", "false");
  const offlineCount = (await canvasInfo(page)).strokes;
  await usePen(page);

  await server.resume();
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await synced(page, server, 2);
  const ops = (await server.operations()).map((o) => envelope(o.envelope));
  expect(ops.map((o) => [o.client_seq, o.operation.type])).toEqual([
    [1, "add_stroke"],
    [2, "erase_region"],
  ]);
  expect(await myStrokeLineage(server, ops[0]!.operation.stroke.stroke_id)).toHaveLength(2);
  await browserMatchesServer(page, server);
  expect((await canvasInfo(page)).strokes).toBe(offlineCount); // no jump when the server caught up
});
