// S3B end-to-end: the browser as a writing replica against the real Rust sync server.
// The harness stands in for the Kindle (it submits envelopes like S2C would).

import { canvasInfo, connect, expect, inkPixels, manifest, test } from "./harness";
import { browserMatchesServer, drawMouse, drawPen, enterWrite, envelope, firstPageStrokes, mapper, opPosts, S1_V7, synced, writes } from "./ink";

test("A: read-only until Write; write mode alone sends nothing; one POST per completed stroke", async ({ page, server, traffic }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  await expect.poll(async () => (await canvasInfo(page)).rendered).toBe(true);
  const before = (await firstPageStrokes(server)).length;

  // Viewing: GET only (S3A preserved) even while the pointer drags over the page.
  await drawMouse(page, [{ x: 3000, y: 3000 }, { x: 5000, y: 4000 }]);
  await page.waitForTimeout(3_000);
  expect(writes(traffic)).toEqual([]);
  expect(await server.head()).toBe(0);

  // Entering write mode alone writes nothing.
  await enterWrite(page);
  await page.waitForTimeout(3_000);
  expect(writes(traffic)).toEqual([]);

  // Pointer down/move send nothing; exactly one POST after pointer up.
  const m = await mapper(page);
  const a = m.toClient({ x: 2000, y: 6000 });
  const b = m.toClient({ x: 7000, y: 9000 });
  await page.mouse.move(a.x, a.y);
  await page.mouse.down();
  await page.mouse.move(b.x, b.y, { steps: 12 });
  await page.waitForTimeout(1_000);
  expect(writes(traffic)).toEqual([]);
  expect(await page.getByTestId("wet-canvas").getAttribute("data-wet")).toBe("1");
  await page.mouse.up();
  await synced(page, server, 1);
  expect(opPosts(traffic)).toHaveLength(1);
  expect(writes(traffic)).toHaveLength(1); // no blob PUT, no genesis POST
  expect(await server.head()).toBe(1);

  // The ledger holds one browser AddStroke: v7 ids, root = stroke, mouse → legacy, canonical coords.
  const ops = await server.operations();
  expect(ops).toHaveLength(1);
  const env = envelope(ops[0]!.envelope);
  expect(env.operation.type).toBe("add_stroke");
  expect(env.client_seq).toBe(1);
  expect(env.replica_id).toMatch(S1_V7);
  expect(env.operation.stroke.stroke_id).toMatch(S1_V7);
  expect(env.operation.stroke.root_id).toBe(env.operation.stroke.stroke_id);
  expect(env.operation.stroke.style).toBe("legacy-fixed-v1");
  const p0 = env.operation.stroke.points[0]!;
  const tol = 2 / m.scale; // two CSS px in canonical units
  expect(Math.abs(p0.x - 2000)).toBeLessThan(tol);
  expect(Math.abs(p0.y - 6000)).toBeLessThan(tol);
  const pl = env.operation.stroke.points[env.operation.stroke.points.length - 1]!;
  expect(Math.abs(pl.x - 7000)).toBeLessThan(tol);
  expect(Math.abs(pl.y - 9000)).toBeLessThan(tol);
  expect(env.operation.stroke.points.every((p) => p.pressure === undefined)).toBe(true);
  expect(env.operation.page_id).toBe((await server.snapshot()).snapshot.notebooks[0]!.pages[0]!.page_id);

  // Server canonical state has it; the browser converges to exactly that snapshot.
  expect((await firstPageStrokes(server)).length).toBe(before + 1);
  await browserMatchesServer(page, server);

  // A stroke starting off the page authors nothing.
  const off = m.toClient({ x: -2000, y: 5000 });
  await page.mouse.move(off.x, off.y);
  await page.mouse.down();
  await page.mouse.move(off.x + 200, off.y + 50, { steps: 5 });
  await page.mouse.up();
  await page.waitForTimeout(1_500);
  expect(opPosts(traffic)).toHaveLength(1);

  // Done leaves write mode; viewing is GET-only again.
  await page.getByTestId("done").click();
  await expect(page.getByTestId("write")).toBeVisible();
  const n = writes(traffic).length;
  await drawMouse(page, [{ x: 3000, y: 12000 }, { x: 6000, y: 13000 }]);
  await page.waitForTimeout(2_000);
  expect(writes(traffic)).toHaveLength(n);
  expect(traffic.consoleErrors).toEqual([]);
});

test("pen: PressureWidthV1 with the real pressure samples", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await drawPen(page, [
    { x: 3000, y: 3000, pressure: 0.2 },
    { x: 4000, y: 3500, pressure: 0.5 },
    { x: 5000, y: 4000, pressure: 0.9 },
  ]);
  await synced(page, server, 1);
  const env = envelope((await server.operations())[0]!.envelope);
  expect(env.operation.stroke.style).toBe("pressure-width-v1");
  // Canonical JSON prints the shortest f32 text; compare as f32.
  expect(env.operation.stroke.points.map((p) => Math.fround(p.pressure!))).toEqual([Math.fround(0.2), Math.fround(0.5), Math.fround(0.9)]);
  await browserMatchesServer(page, server);
});

test("replica identity persists across reload; client_seq continues", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await drawMouse(page, [{ x: 2000, y: 2000 }, { x: 4000, y: 2500 }]);
  await synced(page, server, 1);
  await page.reload();
  await enterWrite(page);
  await drawMouse(page, [{ x: 2000, y: 4000 }, { x: 4000, y: 4500 }]);
  await synced(page, server, 2);
  const envs = (await server.operations()).map((o) => envelope(o.envelope));
  expect(envs.map((e) => e.client_seq)).toEqual([1, 2]);
  expect(envs[1]!.replica_id).toBe(envs[0]!.replica_id);
  // The token is not in IndexedDB.
  const dump = await page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>((ok, fail) => {
      const r = indexedDB.open("scribe-web-sync");
      r.onsuccess = () => ok(r.result);
      r.onerror = () => fail(r.error);
    });
    const out: unknown[] = [];
    for (const name of Array.from(db.objectStoreNames)) {
      out.push(
        await new Promise((ok) => {
          const q = db.transaction(name).objectStore(name).getAll();
          q.onsuccess = () => ok(q.result);
        }),
      );
    }
    db.close();
    return JSON.stringify(out);
  });
  expect(dump).not.toContain(server.token);
  expect(dump).toContain(envs[0]!.replica_id);
});

test("E: offline queue survives tab close; syncs in order on reopen", async ({ page, context, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await drawMouse(page, [{ x: 2000, y: 2000 }, { x: 5000, y: 2500 }]);
  await synced(page, server, 1);
  await browserMatchesServer(page, server); // stroke 1 now comes from the snapshot, not the overlay

  await server.halt();
  const inkBefore = await inkPixels(page);
  await drawMouse(page, [{ x: 2000, y: 5000 }, { x: 5000, y: 5500 }]);
  await drawMouse(page, [{ x: 2000, y: 8000 }, { x: 5000, y: 8500 }]);
  await expect(page.getByTestId("sync-status")).toHaveAttribute("data-phase", "offline", { timeout: 15_000 });
  await expect(page.getByTestId("sync-status")).toHaveText("Offline · 2 pending");
  // The durable overlay shows both queued strokes (layer 2).
  await expect(page.getByTestId("page-canvas")).toHaveAttribute("data-pending", "2");
  expect(await inkPixels(page)).toBeGreaterThan(inkBefore);
  await page.close();

  await server.resume();
  const again = await context.newPage();
  await connect(again, server); // a new tab has no session token
  await synced(again, server, 3);
  const envs = (await server.operations()).map((o) => envelope(o.envelope));
  expect(envs.map((e) => e.client_seq)).toEqual([1, 2, 3]);
  expect(new Set(envs.map((e) => e.replica_id)).size).toBe(1);
  await browserMatchesServer(again, server);
});

test("lost ACK: the same bytes are retried and the server deduplicates", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  const bodies: string[] = [];
  let n = 0;
  await page.route("**/v1/libraries/*/operations", async (route) => {
    const req = route.request();
    if (req.method() !== "POST") return route.continue();
    bodies.push(req.postData() ?? "");
    if (n++ === 0) {
      await route.fetch(); // the server applies it…
      return route.abort("connectionreset"); // …but the ACK never reaches the browser
    }
    return route.continue();
  });
  await enterWrite(page);
  await drawMouse(page, [{ x: 2000, y: 2000 }, { x: 6000, y: 3000 }]);
  await synced(page, server, 1);
  expect(bodies.length).toBeGreaterThanOrEqual(2);
  expect(bodies[1]).toBe(bodies[0]);
  expect(await server.head()).toBe(1);
  const ops = await server.operations();
  expect(ops).toHaveLength(1);
  // Exact bytes: what the browser sent is what the ledger stored.
  expect(bodies[0]).toBe(`{"envelopes":[${ops[0]!.envelope}]}`);
  await browserMatchesServer(page, server);
});

test("two tabs share one replica without sequence collisions", async ({ page, context, server }) => {
  await server.seedFixture();
  const other = await context.newPage();
  await connect(page, server);
  await connect(other, server);
  await enterWrite(page);
  await enterWrite(other);
  await drawMouse(page, [{ x: 2000, y: 2000 }, { x: 5000, y: 2500 }]);
  await drawMouse(other, [{ x: 2000, y: 4000 }, { x: 5000, y: 4500 }]);
  // Truly concurrent commits from both tabs. (Per-page synthetic pen input: Playwright's
  // Firefox driver shares one mouse across pages, so concurrent page.mouse input would mix.)
  await Promise.all([
    drawPen(page, [
      { x: 2000, y: 6000, pressure: 0.5 },
      { x: 5000, y: 6500, pressure: 0.6 },
    ]),
    drawPen(other, [
      { x: 2000, y: 8000, pressure: 0.5 },
      { x: 5000, y: 8500, pressure: 0.6 },
    ]),
  ]);
  await synced(page, server, 4);
  await synced(other, server, 4);
  const envs = (await server.operations()).map((o) => envelope(o.envelope));
  expect(envs.map((e) => e.client_seq).sort((a, b) => a - b)).toEqual([1, 2, 3, 4]);
  expect(new Set(envs.map((e) => e.replica_id)).size).toBe(1);
  expect(new Set(envs.map((e) => e.operation.stroke.stroke_id)).size).toBe(4);
  await browserMatchesServer(page, server);
  await browserMatchesServer(other, server);
});

test("C/D: Kindle strokes arrive while writing; interleaving converges to the server snapshot", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await enterWrite(page);
  await drawMouse(page, [{ x: 2000, y: 2000 }, { x: 5000, y: 2500 }]); // B1
  await synced(page, server, 1);
  await server.pushLiveOp(0); // K1 (AddStroke as the Kindle would send)
  await drawMouse(page, [{ x: 2000, y: 5000 }, { x: 5000, y: 5500 }]); // B2
  await synced(page, server, 3);
  await browserMatchesServer(page, server);
});

test("a semantic refusal fails closed and preserves the pending stroke", async ({ page, server }) => {
  await server.seedFixture();
  await connect(page, server);
  await page.route("**/v1/libraries/*/operations", (route) =>
    route.request().method() === "POST"
      ? route.fulfill({
          status: 422,
          contentType: "application/json",
          body: JSON.stringify({ error: "semantic_refusal", semantic_code: "unknown_page", envelope_index: 0, message: "page never existed" }),
        })
      : route.continue(),
  );
  await enterWrite(page);
  await drawMouse(page, [{ x: 2000, y: 2000 }, { x: 5000, y: 2500 }]);
  await expect(page.getByTestId("sync-conflict")).toContainText("SYNC CONFLICT");
  await expect(page.getByTestId("sync-conflict")).toContainText("Your pending operation has been preserved.");
  await expect(page.getByTestId("sync-status")).toHaveAttribute("data-phase", "conflict");
  await expect(page.getByTestId("sync-status")).toHaveAttribute("data-pending", "1");
  await page.getByTestId("done").click();
  await expect(page.getByTestId("write")).toBeDisabled();
  expect(await server.head()).toBe(0);
  // Durable: still in conflict (and still pending) after reload.
  await page.unrouteAll();
  await page.reload();
  await expect(page.getByTestId("sync-conflict")).toBeVisible();
  await expect(page.getByTestId("sync-status")).toHaveAttribute("data-pending", "1");
  expect(await server.head()).toBe(0);
  expect(manifest.library_id).toBeTruthy();
});
