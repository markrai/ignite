// Shared S3B e2e helpers: canonical<->client mapping, mouse/pen/eraser input, sync waits and
// ledger inspection (used by replica.spec.ts and erase.spec.ts).

import type { Page } from "@playwright/test";
import { canvasInfo, expect, type SyncServer, type Traffic } from "./harness";

export const W = 15624;
export const S1_V7 = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

export type Pt = { x: number; y: number };

/** Canonical → client (CSS px) for the page currently shown. */
export async function mapper(page: Page) {
  await expect.poll(async () => (await canvasInfo(page)).rendered).toBe(true);
  const box = (await page.getByTestId("page-scroller").boundingBox())!;
  const { rect } = await canvasInfo(page);
  const scale = rect[2] / W;
  return {
    scale,
    toClient: (p: Pt) => ({ x: box.x + rect[0] + p.x * scale, y: box.y + rect[1] + p.y * scale }),
    toCanon: (c: Pt) => ({ x: (c.x - box.x - rect[0]) / scale, y: (c.y - box.y - rect[1]) / scale }),
  };
}

export async function drawMouse(page: Page, pts: Pt[]) {
  const m = await mapper(page);
  const c = pts.map(m.toClient);
  await page.mouse.move(c[0]!.x, c[0]!.y);
  await page.mouse.down();
  for (const p of c.slice(1)) await page.mouse.move(p.x, p.y, { steps: 4 });
  await page.mouse.up();
  return c;
}

/** Synthetic pen PointerEvents with real pressure (Playwright has no stylus input). */
export async function drawPen(page: Page, pts: (Pt & { pressure: number })[]) {
  const m = await mapper(page);
  const c = pts.map((p) => ({ ...m.toClient(p), pressure: p.pressure }));
  await page.getByTestId("page-scroller").evaluate((el, samples) => {
    const ev = (type: string, s: { x: number; y: number; pressure: number }) =>
      el.dispatchEvent(
        new PointerEvent(type, {
          pointerId: 7,
          pointerType: "pen",
          isPrimary: true,
          bubbles: true,
          cancelable: true,
          clientX: s.x,
          clientY: s.y,
          pressure: s.pressure,
          button: type === "pointermove" ? -1 : 0,
          buttons: type === "pointerup" ? 0 : 1,
        }),
      );
    ev("pointerdown", samples[0]!);
    for (const s of samples.slice(1)) ev("pointermove", s);
    ev("pointerup", { ...samples[samples.length - 1]!, pressure: 0 });
  }, c);
}

export async function enterWrite(page: Page) {
  await page.getByTestId("write").click();
  await expect(page.getByTestId("pen")).toBeVisible();
  await expect(page.getByTestId("done")).toBeVisible();
}

export async function enterType(page: Page) {
  await page.getByTestId("type").click();
  await expect(page.getByTestId("done")).toBeVisible();
}

/** Synthetic touch pointers on the page scroller (what a touch screen delivers). */
export async function touch(page: Page, type: "pointerdown" | "pointermove" | "pointerup", id: number, x: number, y: number) {
  await page.getByTestId("page-scroller").evaluate(
    (el, a) => {
      const r = el.getBoundingClientRect();
      el.dispatchEvent(
        new PointerEvent(a.type, {
          pointerId: a.id,
          pointerType: "touch",
          isPrimary: a.id === 1,
          clientX: r.left + a.x,
          clientY: r.top + a.y,
          bubbles: true,
          cancelable: true,
          buttons: a.type === "pointerup" ? 0 : 1,
        }),
      );
    },
    { type, id, x, y },
  );
}

/** Pinch from spread `d0` to `d1` (px) around viewport point (cx, cy), in `steps` moves. */
export async function pinch(page: Page, cx: number, cy: number, d0: number, d1: number, steps = 8) {
  await touch(page, "pointerdown", 1, cx - d0 / 2, cy);
  await touch(page, "pointerdown", 2, cx + d0 / 2, cy);
  for (let i = 1; i <= steps; i++) {
    const d = d0 + ((d1 - d0) * i) / steps;
    await touch(page, "pointermove", 1, cx - d / 2, cy);
    await touch(page, "pointermove", 2, cx + d / 2, cy);
  }
  await touch(page, "pointerup", 1, cx - d1 / 2, cy);
  await touch(page, "pointerup", 2, cx + d1 / 2, cy);
}

/** Wait until the server has `head` operations AND this tab reports nothing pending. */
export async function synced(page: Page, server: SyncServer, head: number) {
  await expect.poll(() => server.head(), { timeout: 20_000 }).toBe(head);
  await expect(page.getByTestId("sync-status")).toHaveAttribute("data-phase", "synced", { timeout: 20_000 });
  await expect(page.getByTestId("sync-status")).toHaveAttribute("data-pending", "0");
}

export function writes(t: Traffic) {
  return t.requests.filter((r) => new URL(r.url).pathname.startsWith("/v1") && r.method !== "GET");
}

export function opPosts(t: Traffic) {
  return writes(t).filter((r) => r.method === "POST" && /\/v1\/libraries\/[^/]+\/operations$/.test(new URL(r.url).pathname));
}

export async function firstPageStrokes(server: SyncServer) {
  const s = await server.snapshot();
  return s.snapshot.notebooks[0]!.pages[0]!.strokes;
}

/** The browser shows exactly the server's current canonical page (no pending overlay). */
export async function browserMatchesServer(page: Page, server: SyncServer) {
  const snap = await server.snapshot();
  await expect(page.getByTestId("status")).toHaveAttribute("data-snapshot", snap.snapshot_sha256, { timeout: 20_000 });
  await expect(page.getByTestId("status")).toHaveAttribute("data-head", String(snap.through_server_seq));
  await expect.poll(async () => (await canvasInfo(page)).strokes).toBe(snap.snapshot.notebooks[0]!.pages[0]!.strokes.length);
  await expect(page.getByTestId("page-canvas")).toHaveAttribute("data-pending", "0");
}

export function envelope(text: string) {
  return JSON.parse(text) as {
    replica_id: string;
    client_seq: number;
    operation_id: string;
    operation: {
      type: string;
      notebook_id?: string;
      page_id?: string;
      root_id?: string;
      stroke: { stroke_id: string; root_id: string; style: string; points: { x: number; y: number; pressure?: number }[] };
    };
  };
}

