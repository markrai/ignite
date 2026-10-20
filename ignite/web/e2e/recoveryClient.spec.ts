// Slice 1H-A: clean browser recovery after a portable backup restore, end to end against two
// independent, disposable `scribe-server` instances. A source server holds two synthetic
// libraries; its real backup export is restored, through the real upload + explicit
// confirmation, into an empty recovery server; then a genuinely fresh browser (new context: no
// cookies, Web Storage, IndexedDB replica, service worker or remembered library) requests
// access the normal product way, finds the restored libraries, renders their actual content,
// writes one stroke, syncs it, and reopens it.
//
// What a restore changes and what it keeps (the established contract): every restored library
// gets a NEW library id and starts at server sequence 0 with the backed-up snapshot as its
// genesis. Notebook, page, stroke, text, link and folder ids, titles, geometry and background
// blob references are kept byte-for-byte; only `library_id` (and therefore the snapshot hash)
// changes. No operation history is carried over.

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import type { BrowserContext, Page, Request } from "@playwright/test";
import { canvasInfo, inkPixels, manifest, snapshotText } from "./harness";
import { drawMouse, enterWrite, S1_V7 } from "./ink";
import { expect, test, type ProductServer } from "./product";

const EXPORTS = "/control/v1/admin/backup/exports";
const RESTORE_JOBS = "/control/v1/admin/restore/jobs";
const FIX_DIR = new URL("../tests/fixtures/", import.meta.url);

// Fixture identities (tests/fixtures/snapshot-v1.json, snapshot-b-v1.json).
const FIELD_NOTES = "01a14ab8-8368-75bf-ae2e-f7fd5cc05bf9";
const FIELD_P1 = "01a14ab8-8368-75bf-ae2e-f7fe0973fe1f";
const SKETCHES = "01a14ab8-8368-75bf-ae2e-f801a8d1d755";
const SKETCH_P1 = "01a14ab8-8368-75bf-ae2e-f802d3bc2368";
const SKETCH_P2 = "01a14ab8-8368-75bf-ae2e-f80364682aee";
const LIB_A = manifest.library_id;
const LIB_B = manifest.second_library.library_id;
// Identities added to library A for this slice (fresh UUIDv7 text).
const FOLDER = "01a14ab8-8368-75bf-ae2e-f8f000000001";
const TEXT = "01a14ab8-8368-75bf-ae2e-f8f000000002";
const LINK = "01a14ab8-8368-75bf-ae2e-f8f000000003";
const TEXT_CONTENT = "Recovered notes: page one";

interface Lib {
  library_id: string;
  head_server_seq: number;
  snapshot_sha256: string;
  notebook_count: number;
  page_count: number;
}

interface SnapshotDoc {
  library_id: string;
  through_server_seq: number;
  snapshot_sha256: string;
  snapshot: {
    library_id: string;
    folders?: { folder_id: string; title: string }[];
    notebooks: {
      notebook_id: string;
      title: string;
      folder_id?: string;
      pages: { page_id: string; background: { kind: string; sha256?: string }; strokes: { stroke_id: string }[]; texts?: { text_id: string; content: string }[]; links?: { link_id: string }[] }[];
    }[];
  };
}

/** Index just past the JSON value that starts at `start` (an object or array). */
function valueEnd(text: string, start: number): number {
  let depth = 0;
  let inStr = false;
  for (let i = start; i < text.length; i++) {
    const ch = text[i];
    if (inStr) {
      if (ch === "\\") i++;
      else if (ch === '"') inStr = false;
    } else if (ch === '"') inStr = true;
    else if (ch === "{" || ch === "[") depth++;
    else if ((ch === "}" || ch === "]") && --depth === 0) return i + 1;
  }
  throw new Error("unterminated JSON value");
}

/** Insert `extra` (fields) just before the closing brace of the object that starts with `head`. */
function addFields(text: string, head: string, extra: string): string {
  const at = text.indexOf(head);
  expect(at, head).toBeGreaterThan(-1);
  const end = valueEnd(text, at);
  return text.slice(0, end - 1) + extra + text.slice(end - 1);
}

/**
 * Library A: the canonical fixture plus a folder (holding "Sketches"), one text object and one
 * page link on Field Notes page 1, in the protocol's field order with f32 values written as the
 * canonical serializer writes them. The server accepts a genesis only if these exact bytes are
 * its own canonical serialization of a valid snapshot, so nothing malformed can get in.
 */
function richLibraryA(): string {
  let s = snapshotText;
  s = addFields(
    s,
    `{"page_id":"${FIELD_P1}"`,
    `,"texts":[{"text_id":"${TEXT}","x":1200.0,"y":11000.0,"width":9000.0,"font_size":480.0,"face":"default-sans-v1","content":"${TEXT_CONTENT}"}]` +
      `,"links":[{"link_id":"${LINK}","source_notebook_id":"${FIELD_NOTES}","source_page_id":"${FIELD_P1}","source_x":12500.0,"source_y":2500.0,"target_notebook_id":"${SKETCHES}","target_page_id":"${SKETCH_P1}"}]`,
  );
  s = addFields(s, `{"notebook_id":"${SKETCHES}"`, `,"folder_id":"${FOLDER}"`);
  s = s.slice(0, s.length - 1) + `,"folders":[{"folder_id":"${FOLDER}","title":"Recovered Journals"}]}`;
  return s;
}

async function libraries(server: ProductServer, auth: { bearer?: string; cookie?: string }): Promise<Lib[]> {
  const r = await server.call("GET", "/v1/libraries", auth);
  expect(r.status).toBe(200);
  return (r.json as { libraries: Lib[] }).libraries;
}

async function snapshotOf(server: ProductServer, auth: { bearer?: string; cookie?: string }, lib: string): Promise<SnapshotDoc> {
  const r = await server.call("GET", `/v1/libraries/${lib}/snapshot`, auth);
  expect(r.status).toBe(200);
  return r.json as SnapshotDoc;
}

async function operations(server: ProductServer, cookie: string, lib: string): Promise<{ server_seq: number; envelope: string }[]> {
  const r = await server.call("GET", `/v1/libraries/${lib}/operations?after=0&limit=500`, { cookie });
  expect(r.status).toBe(200);
  return (r.json as { operations: { server_seq: number; envelope: unknown }[] }).operations.map((o) => ({
    server_seq: o.server_seq,
    envelope: typeof o.envelope === "string" ? o.envelope : JSON.stringify(o.envelope),
  }));
}

/** Everything a source server holds that a recovery must leave untouched. */
async function ledgerOf(server: ProductServer, bearer: string): Promise<unknown> {
  const libs = await libraries(server, { bearer });
  return Promise.all(libs.map(async (l) => [l, await snapshotOf(server, { bearer }, l.library_id)]));
}

/** The source server's real portable backup (export API, the same one the browser uses). */
async function exportFrom(server: ProductServer, admin: string, path: string): Promise<{ sha256: string; bytes: number }> {
  const start = await server.call("POST", EXPORTS, { cookie: admin });
  expect(start.status).toBe(202);
  const id = (start.json as { export_id: string }).export_id;
  let ready: { archive: { sha256: string; bytes: number } } | null = null;
  await expect
    .poll(
      async () => {
        const s = (await server.call("GET", `${EXPORTS}/${id}`, { cookie: admin })).json as { state: string; archive: { sha256: string; bytes: number } };
        if (s.state === "ready") ready = s;
        return s.state;
      },
      { timeout: 20_000 },
    )
    .toBe("ready");
  const r = await fetch(`${server.url}${EXPORTS}/${id}/download`, { headers: { Cookie: `scribe_session=${admin}` } });
  expect(r.status).toBe(200);
  const bytes = new Uint8Array(await r.arrayBuffer());
  writeFileSync(path, bytes);
  const archive = ready!.archive;
  expect(bytes.length).toBe(archive.bytes);
  expect(createHash("sha256").update(readFileSync(path)).digest("hex")).toBe(archive.sha256);
  return archive;
}

/** Upload, then explicitly confirm the exact verified plan; wait for the authoritative result. */
async function restoreInto(server: ProductServer, admin: string, zip: Uint8Array): Promise<Map<string, string>> {
  const up = await server.call("POST", RESTORE_JOBS, { cookie: admin, body: zip, contentType: "application/zip" });
  expect(up.status, JSON.stringify(up.json)).toBe(201);
  const job = up.json as { job_id: string; confirmation: string; archive: { sha256: string }; libraries: { source_library_id: string; destination_library_id: string }[] };
  const confirmed = await server.call("POST", `${RESTORE_JOBS}/${job.job_id}/confirm`, {
    cookie: admin,
    body: {
      v: 1,
      confirmation: job.confirmation,
      archive_sha256: job.archive.sha256,
      destinations: job.libraries.map((l) => ({ source_library_id: l.source_library_id, destination_library_id: l.destination_library_id })),
    },
  });
  expect(confirmed.status).toBe(202);
  await expect
    .poll(async () => ((await server.call("GET", `${RESTORE_JOBS}/${job.job_id}`, { cookie: admin })).json as { state: string }).state, { timeout: 20_000 })
    .toBe("succeeded");
  return new Map(job.libraries.map((l) => [l.source_library_id, l.destination_library_id]));
}

/** A brand-new browser profile, granted access the normal way (request → admin approves the code). */
async function freshBrowser(context: BrowserContext, server: ProductServer, admin: string, name: string): Promise<Page> {
  expect(await context.cookies()).toEqual([]);
  const page = await context.newPage();
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(server.url + "/");
  // Nothing local: no Web Storage, no replica database, no service worker, and the app's own
  // preference database (created on startup) holds nothing.
  const local = await page.evaluate(async () => {
    const names = indexedDB.databases ? (await indexedDB.databases()).map((d) => d.name ?? "") : [];
    let prefs = 0;
    for (const name of names) {
      const db = await new Promise<IDBDatabase>((ok, fail) => {
        const r = indexedDB.open(name);
        r.onsuccess = () => ok(r.result);
        r.onerror = () => fail(r.error);
      });
      for (const store of Array.from(db.objectStoreNames)) {
        prefs += await new Promise<number>((ok) => {
          const q = db.transaction(store).objectStore(store).count();
          q.onsuccess = () => ok(q.result);
          q.onerror = () => ok(0);
        });
      }
      db.close();
    }
    return {
      storage: localStorage.length + sessionStorage.length,
      replicaDb: names.includes("scribe-web-sync"),
      rows: prefs,
      sw: navigator.serviceWorker ? (await navigator.serviceWorker.getRegistrations()).length : 0,
    };
  });
  expect(local).toEqual({ storage: 0, replicaDb: false, rows: 0, sw: 0 });
  await expect(page.getByTestId("request-heading")).toBeVisible();
  await page.getByTestId("browser-name").fill(name);
  await page.getByTestId("request-access").click();
  const code = (await page.getByTestId("comparison-code").textContent())!.trim();
  const pending = (await server.call("GET", "/control/v1/admin/pairing", { cookie: admin })).json as { requests: { request_id: string; comparison_code: string }[] };
  const req = pending.requests.find((r) => r.comparison_code === code)!;
  expect((await server.call("POST", `/control/v1/admin/pairing/${req.request_id}/approve`, { cookie: admin })).status).toBe(200);
  await expect(page.getByTestId("library-chooser")).toBeVisible({ timeout: 20_000 });
  return page;
}

/** IndexedDB replica state (`scribe-web-sync`), read only to check its invariants. */
async function replicaState(page: Page): Promise<{ replica: { library_id: string; replica_id: string; last_client_seq: string; acked_client_seq: string; conflict: unknown }[]; outbox: { library_id: string; client_seq: string; text: string }[]; accepted: { library_id: string }[] }> {
  return page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>((ok, fail) => {
      const r = indexedDB.open("scribe-web-sync");
      r.onsuccess = () => ok(r.result);
      r.onerror = () => fail(r.error);
    });
    const all = (name: string) =>
      db.objectStoreNames.contains(name)
        ? new Promise<unknown[]>((ok) => {
            const q = db.transaction(name).objectStore(name).getAll();
            q.onsuccess = () => ok(q.result as unknown[]);
          })
        : Promise.resolve([]);
    const out = { replica: await all("replica"), outbox: await all("outbox"), accepted: await all("accepted") };
    db.close();
    return out as never;
  });
}

async function openLibrary(page: Page, lib: string) {
  await expect(page.getByTestId("library-chooser")).toBeVisible();
  await page.locator(`[data-testid="library-card"][title="${lib}"]`).click();
  await expect(page.getByTestId("notebook-item").first()).toBeVisible({ timeout: 20_000 });
}

async function openNotebook(page: Page, title: string) {
  await page.getByTestId("notebooks").locator('[data-testid="notebook-item"]', { hasText: title }).click();
}

async function showsPage(page: Page, pageId: string, strokes: number) {
  await expect.poll(async () => (await canvasInfo(page)).pageId, { timeout: 20_000 }).toBe(pageId);
  await expect.poll(async () => (await canvasInfo(page)).rendered, { timeout: 20_000 }).toBe(true);
  await expect.poll(async () => (await canvasInfo(page)).strokes).toBe(strokes);
}

/** Console errors, uncaught exceptions, failed requests and HTTP errors seen by a page. */
function watch(page: Page) {
  const problems: string[] = [];
  const ops: Request[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") problems.push(`console: ${m.text()}`);
  });
  page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));
  page.on("requestfailed", (r) => problems.push(`failed: ${r.method()} ${r.url()} ${r.failure()?.errorText ?? ""}`));
  page.on("response", (r) => {
    if (r.status() >= 400 && new URL(r.url()).pathname.startsWith("/v1")) problems.push(`http ${r.status()}: ${r.request().method()} ${r.url()}`);
  });
  page.on("request", (r) => {
    if (r.method() === "POST" && /\/v1\/libraries\/[^/]+\/operations$/.test(new URL(r.url()).pathname)) ops.push(r);
  });
  return { problems, ops };
}

test("a fresh browser discovers, renders, edits, syncs and reopens libraries restored onto an independent server", async ({ browser, product, recovery }, testInfo) => {
  test.setTimeout(180_000);

  // ---- Source server: two synthetic libraries, seeded as the Kindle would.
  const sourceAdmin = await product.claimAsHarness();
  const kindle = await product.pairKindle(sourceAdmin);
  const blob = new Uint8Array(readFileSync(new URL(manifest.blob.file, FIX_DIR)));
  expect((await product.call("PUT", `/v1/blobs/${manifest.blob.sha256}`, { bearer: kindle, body: blob, csrf: false })).status).toBeLessThan(300);
  const libA = richLibraryA();
  const posted = await product.call("POST", "/v1/libraries", { bearer: kindle, body: libA, csrf: false });
  expect(posted.status, JSON.stringify(posted.json)).toBe(201); // canonical and valid, or refused
  const libB = readFileSync(new URL(manifest.second_library.snapshot, FIX_DIR), "utf8");
  expect((await product.call("POST", "/v1/libraries", { bearer: kindle, body: libB, csrf: false })).status).toBe(201);
  const sourceBefore = await ledgerOf(product, kindle);
  const srcA = await snapshotOf(product, { bearer: kindle }, LIB_A);
  const srcB = await snapshotOf(product, { bearer: kindle }, LIB_B);
  expect(srcA.snapshot_sha256).toBe(createHash("sha256").update(libA).digest("hex"));
  // The fixture really has what the recovery must carry.
  const a = srcA.snapshot;
  expect(a.folders).toEqual([{ folder_id: FOLDER, title: "Recovered Journals" }]);
  expect(a.notebooks.find((n) => n.notebook_id === SKETCHES)!.folder_id).toBe(FOLDER);
  const fieldP1 = a.notebooks[0]!.pages[0]!;
  expect([fieldP1.page_id, fieldP1.strokes.length, fieldP1.texts![0]!.content, fieldP1.links![0]!.link_id]).toEqual([FIELD_P1, 5, TEXT_CONTENT, LINK]);
  const blobPages = a.notebooks.flatMap((n) => n.pages).filter((p) => p.background.kind === "blob");
  expect(blobPages.map((p) => p.background.sha256)).toEqual([manifest.blob.sha256, manifest.blob.sha256]); // one blob, used twice

  // ---- Export: the real portable backup, saved to a disposable file.
  const zipPath = testInfo.outputPath("source-backup.zip");
  const archive = await exportFrom(product, sourceAdmin, zipPath);

  // ---- Recovery server: independent, initially empty; real upload + explicit confirmation.
  const recoveryAdmin = await recovery.claimAsHarness();
  expect(await recovery.serverId()).not.toBe(await product.serverId());
  expect(recovery.url).not.toBe(product.url);
  expect(await libraries(recovery, { cookie: recoveryAdmin })).toEqual([]);
  const mapping = await restoreInto(recovery, recoveryAdmin, new Uint8Array(readFileSync(zipPath)));
  const dstA = mapping.get(LIB_A)!;
  const dstB = mapping.get(LIB_B)!;
  expect([...mapping.keys()].sort()).toEqual([LIB_A, LIB_B].sort());
  for (const d of [dstA, dstB]) {
    expect(d).toMatch(S1_V7);
    expect([LIB_A, LIB_B]).not.toContain(d);
  }
  testInfo.annotations.push({ type: "mapping", description: `${LIB_A} → ${dstA}; ${LIB_B} → ${dstB}; archive ${archive.sha256} (${archive.bytes} bytes)` });
  const restored = await libraries(recovery, { cookie: recoveryAdmin });
  expect(restored.map((l) => l.library_id).sort()).toEqual([dstA, dstB].sort());
  for (const l of restored) expect(l.head_server_seq).toBe(0);
  // The restore contract: only the library id (and with it the snapshot hash) changes.
  const rA = await snapshotOf(recovery, { cookie: recoveryAdmin }, dstA);
  const rB = await snapshotOf(recovery, { cookie: recoveryAdmin }, dstB);
  expect(rA.snapshot).toEqual({ ...srcA.snapshot, library_id: dstA });
  expect(rB.snapshot).toEqual({ ...srcB.snapshot, library_id: dstB });
  expect(rA.snapshot_sha256).not.toBe(srcA.snapshot_sha256);
  expect(await operations(recovery, recoveryAdmin, dstA)).toEqual([]); // no history carried over
  const restoredB = rB.snapshot_sha256;

  // ---- A genuinely fresh browser on the recovery server.
  const ctx = await browser.newContext();
  const page = await freshBrowser(ctx, recovery, recoveryAdmin, "Fresh recovery browser");
  const seen = watch(page);
  const cards = page.getByTestId("library-card");
  await expect(cards).toHaveCount(2);
  const titles = await cards.evaluateAll((els) => els.map((e) => e.getAttribute("title")));
  expect(titles.sort()).toEqual([dstA, dstB].sort());
  expect(await page.content()).not.toContain(LIB_A); // the source ids are not presented
  for (const l of restored) {
    await expect(page.locator(`[data-testid="library-card"][title="${l.library_id}"]`)).toContainText(
      `${l.notebook_count} ${l.notebook_count === 1 ? "notebook" : "notebooks"} · ${l.page_count} ${l.page_count === 1 ? "page" : "pages"}`,
    );
  }

  // Library A renders its real content: ink, text, link, folder, backgrounds.
  await openLibrary(page, dstA);
  const sidebar = page.getByTestId("notebooks");
  const folder = sidebar.getByTestId("folder-item");
  await expect(folder).toHaveCount(1);
  await expect(folder).toContainText("Recovered Journals");
  await folder.getByTestId("folder-toggle").click();
  await expect(folder.getByTestId("notebook-item")).toHaveText(/Sketches/);
  await openNotebook(page, "Field Notes");
  await showsPage(page, FIELD_P1, 5);
  expect(await inkPixels(page)).toBeGreaterThan(0);
  await expect(page.getByTestId("status")).toHaveAttribute("data-snapshot", rA.snapshot_sha256);
  await expect(page.getByTestId("status")).toHaveAttribute("data-head", "0");
  await expect(page.locator(`[data-testid="page-text"][data-text-id="${TEXT}"]`)).toContainText(TEXT_CONTENT);
  const link = page.locator(`[data-testid="page-link"][data-link-id="${LINK}"]`);
  await expect(link).toBeVisible();
  // The link jumps to the background page; the restored blob is fetched and verified.
  const [blobResp] = await Promise.all([page.waitForResponse((r) => new URL(r.url()).pathname === `/v1/blobs/${manifest.blob.sha256}`), link.click()]);
  expect(blobResp.status()).toBe(200);
  await showsPage(page, SKETCH_P1, 1);
  expect((await canvasInfo(page)).background).toBe("blob");
  await page.getByTestId("next").click();
  await showsPage(page, SKETCH_P2, 0); // the same blob, referenced a second time
  expect((await canvasInfo(page)).background).toBe("blob");

  // The writable replica starts empty: nothing imported, nothing pending, no conflict.
  await expect(page.getByTestId("write")).toBeEnabled();
  let idb = await replicaState(page);
  expect(idb).toEqual({ replica: [], outbox: [], accepted: [] });
  await expect(page.getByTestId("sync-conflict")).toHaveCount(0);

  // ---- One real edit on Field Notes page 1.
  await openNotebook(page, "Field Notes");
  await showsPage(page, FIELD_P1, 5);
  await enterWrite(page);
  await drawMouse(page, [
    { x: 2000, y: 16000 },
    { x: 6000, y: 16600 },
  ]);
  await expect.poll(async () => (await libraries(recovery, { cookie: recoveryAdmin })).find((l) => l.library_id === dstA)!.head_server_seq, { timeout: 20_000 }).toBe(1);
  await expect(page.getByTestId("sync-status")).toHaveAttribute("data-phase", "synced", { timeout: 20_000 });
  await expect(page.getByTestId("sync-status")).toHaveAttribute("data-pending", "0");
  await expect(page.getByTestId("sync-conflict")).toHaveCount(0);
  // Exactly one operation, from a new replica, for the destination library only.
  expect(seen.ops).toHaveLength(1);
  expect(new URL(seen.ops[0]!.url()).pathname).toBe(`/v1/libraries/${dstA}/operations`);
  const ops = await operations(recovery, recoveryAdmin, dstA);
  expect(ops).toHaveLength(1);
  const env = JSON.parse(ops[0]!.envelope) as { replica_id: string; client_seq: number; operation: { type: string; page_id: string } };
  expect([ops[0]!.server_seq, env.client_seq, env.operation.type, env.operation.page_id]).toEqual([1, 1, "add_stroke", FIELD_P1]);
  expect(env.replica_id).toMatch(S1_V7);
  idb = await replicaState(page);
  expect(idb.replica.map((r) => [r.library_id, r.replica_id, r.conflict])).toEqual([[dstA, env.replica_id, null]]);
  expect(BigInt(idb.replica[0]!.last_client_seq)).toBe(1n);
  expect(BigInt(idb.replica[0]!.acked_client_seq)).toBe(1n);
  expect(idb.outbox).toEqual([]);
  expect(JSON.stringify(idb)).not.toContain(LIB_A);
  expect(JSON.stringify(idb)).not.toContain(LIB_B);
  // The server's snapshot has the edit next to everything restored.
  const after = await snapshotOf(recovery, { cookie: recoveryAdmin }, dstA);
  expect(after.through_server_seq).toBe(1);
  const p1 = after.snapshot.notebooks[0]!.pages[0]!;
  expect(p1.strokes.map((s) => s.stroke_id).slice(0, 5)).toEqual(fieldP1.strokes.map((s) => s.stroke_id));
  expect(p1.strokes).toHaveLength(6);
  expect(p1.texts).toEqual(rA.snapshot.notebooks[0]!.pages[0]!.texts);
  expect(p1.links).toEqual(rA.snapshot.notebooks[0]!.pages[0]!.links);
  expect(after.snapshot.folders).toEqual(rA.snapshot.folders); // folders untouched
  expect(after.snapshot.notebooks.map((n) => [n.notebook_id, n.title, n.folder_id])).toEqual(rA.snapshot.notebooks.map((n) => [n.notebook_id, n.title, n.folder_id]));
  for (const [i, nb] of after.snapshot.notebooks.entries()) {
    for (const [j, pg] of nb.pages.entries()) if (i > 0 || j > 0) expect(pg).toEqual(rA.snapshot.notebooks[i]!.pages[j]);
  }
  await page.getByTestId("done").click();
  await expect(page.getByTestId("status")).toHaveAttribute("data-snapshot", after.snapshot_sha256, { timeout: 20_000 });
  await showsPage(page, FIELD_P1, 6);

  // ---- Switch to library B and back: separate state, nothing mixed.
  await page.getByRole("button", { name: "Libraries" }).click();
  await openLibrary(page, dstB);
  await showsPage(page, rB.snapshot.notebooks[0]!.pages[0]!.page_id, 1);
  await expect(page.getByTestId("status")).toHaveAttribute("data-snapshot", restoredB);
  await page.getByRole("button", { name: "Libraries" }).click();
  await openLibrary(page, dstA);
  await openNotebook(page, "Field Notes");
  await showsPage(page, FIELD_P1, 6);

  // ---- Reload: the edit persists; the outbox is acknowledged and compacted.
  await page.reload();
  await openLibrary(page, dstA);
  await openNotebook(page, "Field Notes");
  await showsPage(page, FIELD_P1, 6);
  await expect(page.locator(`[data-testid="page-text"][data-text-id="${TEXT}"]`)).toContainText(TEXT_CONTENT);
  await expect(page.getByTestId("status")).toHaveAttribute("data-snapshot", after.snapshot_sha256);
  await expect.poll(async () => (await replicaState(page)).accepted.length, { timeout: 20_000 }).toBe(0);
  idb = await replicaState(page);
  expect(idb.outbox).toEqual([]);
  expect(idb.replica.filter((r) => r.library_id === dstA).map((r) => r.replica_id)).toEqual([env.replica_id]); // same replica, not re-minted
  expect(idb.replica.every((r) => r.library_id === dstA || r.library_id === dstB)).toBe(true);
  expect(seen.ops.every((r) => new URL(r.url()).pathname === `/v1/libraries/${dstA}/operations`)).toBe(true);

  // Library B was not modified.
  const bNow = await snapshotOf(recovery, { cookie: recoveryAdmin }, dstB);
  expect([bNow.through_server_seq, bNow.snapshot_sha256]).toEqual([0, restoredB]);
  expect(await operations(recovery, recoveryAdmin, dstB)).toEqual([]);
  expect(seen.problems).toEqual([]);

  // ---- A second fresh browser sees the edit from the server alone.
  const ctx2 = await browser.newContext();
  const second = await freshBrowser(ctx2, recovery, recoveryAdmin, "Second fresh browser");
  const seen2 = watch(second);
  await openLibrary(second, dstA);
  await openNotebook(second, "Field Notes");
  await showsPage(second, FIELD_P1, 6);
  await expect(second.getByTestId("status")).toHaveAttribute("data-snapshot", after.snapshot_sha256);
  expect(await inkPixels(second)).toBeGreaterThan(0);
  expect(await replicaState(second)).toEqual({ replica: [], outbox: [], accepted: [] }); // viewing mints nothing
  expect(seen2.problems).toEqual([]);

  // ---- The source server never changed.
  expect(await ledgerOf(product, kindle)).toEqual(sourceBefore);
  await ctx.close();
  await ctx2.close();
});
