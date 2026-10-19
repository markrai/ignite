// Slice 1G-A: Server Settings → Backup & Restore (read-only) against the real `scribe-server`.
// The browser only ever reads restore jobs. The HARNESS prepares and confirms them over the
// real control API (uploading and confirming from the browser are later slices). Playwright's
// browser connects over loopback, which the restore routes require.

import type { BrowserContext, Page, Request } from "@playwright/test";
import { connect, manifest, snapshotText, test as devTest } from "./harness";
import { asAdmin, expect, test, type ProductServer } from "./product";

interface Prepared {
  job_id: string;
  confirmation: string;
  archive: { sha256: string };
  libraries: { source_library_id: string; destination_library_id: string }[];
}

/** Every request the page makes to the restore routes. */
function restoreTraffic(page: Page): Request[] {
  const seen: Request[] = [];
  page.on("request", (r) => {
    if (new URL(r.url()).pathname.startsWith("/control/v1/admin/restore")) seen.push(r);
  });
  return seen;
}

/** Optional screenshots of the section (Settings scrolls inside its own container). */
async function shot(page: Page, name: string) {
  const dir = process.env.RESTORE_SCREENSHOT_DIR;
  if (dir) await page.getByTestId("restore-card").screenshot({ path: `${dir}/${name}.png` });
}

/** A member browser session (access requested, approved by the admin) as a cookie value. */
async function memberCookie(product: ProductServer, adminCookie: string): Promise<string> {
  const c = (await product.call("POST", "/control/v1/pairing", { body: { client_type: "browser", display_name: "Member browser" } })).json as {
    request_id: string;
    request_secret: string;
  };
  expect((await product.call("POST", `/control/v1/admin/pairing/${c.request_id}/approve`, { cookie: adminCookie })).status).toBe(200);
  const p = await product.call("POST", `/control/v1/pairing/${c.request_id}/poll`, { body: { request_secret: c.request_secret } });
  return /scribe_session=([^;]+)/.exec(p.setCookie!)![1]!;
}

test("an administrator sees prepared and completed restores read-only in Server Settings", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  const traffic = restoreTraffic(page);

  // Reached through the normal admin entry point (the Settings button), not only by URL.
  await page.goto(product.url + "/");
  await page.getByTestId("settings").click();
  await expect(page).toHaveURL(/\/settings\/server$/);
  const card = page.getByTestId("restore-card");
  await expect(card.getByRole("heading", { name: "Backup & Restore" })).toBeVisible();
  await expect(card.getByTestId("restore-empty")).toContainText("No restore jobs yet.");
  await expect(page.getByTestId("server-card")).toBeVisible(); // the rest of Settings still works

  // The harness prepares a job from a real Slice 1C export (the UI cannot upload yet).
  const zip = product.exportBackup(kindle);
  const up = await product.call("POST", "/control/v1/admin/restore/jobs", { cookie: admin, body: zip, contentType: "application/zip" });
  expect(up.status).toBe(201);
  const prepared = up.json as Prepared;
  const lib = prepared.libraries[0]!;

  await card.getByTestId("restore-refresh").click();
  const row = card.getByTestId("restore-job");
  await expect(row).toHaveAttribute("data-state", "ready");
  await expect(row.getByTestId("restore-state")).toHaveText("Verified, awaiting confirmation");
  // Prepared by the harness: this browser never held its one-time confirmation.
  await expect(row).toContainText("this browser no longer has its one-time confirmation");
  await expect(row.getByTestId("restore-review")).toHaveCount(0);
  await row.getByTestId("restore-toggle").click();
  await expect(row.getByTestId("restore-toggle")).toHaveAttribute("aria-expanded", "true");
  await expect(row.getByTestId("restore-source")).toHaveText(lib.source_library_id);
  await expect(row.getByTestId("restore-destination")).toHaveText(lib.destination_library_id);
  await expect(row.getByTestId("restore-details")).toContainText("Proposed new library");
  // Never a confirm or download control (cancelling a prepared restore arrived in 1G-B).
  await expect(row.getByRole("button", { name: /confirm|restore now|download/i })).toHaveCount(0);
  expect(await page.content()).not.toContain(prepared.confirmation);
  await page.setViewportSize({ width: 1280, height: 900 });
  await shot(page, "restore-ready-desktop");
  await page.setViewportSize({ width: 390, height: 844 });
  await shot(page, "restore-ready-mobile");
  // Long identifiers wrap: nothing scrolls the page sideways on a phone.
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.setViewportSize({ width: 1280, height: 900 });

  // The harness confirms; the page reads the outcome.
  const confirmed = await product.call("POST", `/control/v1/admin/restore/jobs/${prepared.job_id}/confirm`, {
    cookie: admin,
    body: {
      v: 1,
      confirmation: prepared.confirmation,
      archive_sha256: prepared.archive.sha256,
      destinations: prepared.libraries.map((l) => ({ source_library_id: l.source_library_id, destination_library_id: l.destination_library_id })),
    },
  });
  expect(confirmed.status).toBe(202);
  await expect
    .poll(async () => {
      await card.getByTestId("restore-refresh").click();
      return row.getAttribute("data-state");
    })
    .toBe("succeeded");
  await expect(row.getByTestId("restore-publication")).toHaveAttribute("data-publication", "complete");
  await expect(row.getByTestId("restore-details")).toContainText("Restored as");
  await expect(row.getByTestId("restore-destination")).toHaveText(lib.destination_library_id);
  expect(await page.content()).not.toContain(prepared.confirmation);
  await shot(page, "restore-succeeded-desktop");
  await page.setViewportSize({ width: 390, height: 844 });
  await shot(page, "restore-succeeded-mobile");

  // The browser only ever read: GETs to the restore routes, nothing else.
  expect(traffic.length).toBeGreaterThan(0);
  expect(traffic.map((r) => r.method()).filter((m) => m !== "GET")).toEqual([]);
  for (const r of traffic) expect(await r.headerValue("authorization")).toBeNull();
});

test("restore refusals are explained and never leave privileged data on screen", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  await asAdmin(context, product, admin);
  const job = {
    v: 1,
    job_id: "01a0de1f-0000-7000-8000-00000000000a",
    state: "failed",
    usable: false,
    cleanup_pending: false,
    expires_in_s: 600,
    archive: { sha256: "ab".repeat(32), bytes: 2048, format: "ignite-portable-backup", format_version: 1, snapshot_version: 1 },
    scope: "current_state_only",
    not_restored: [],
    totals: { libraries: 1, notebooks: 1, pages: 1, blobs: 0, blob_bytes: 0 },
    libraries: [
      {
        source_library_id: "01a0de1f-0000-7000-8000-000000000001",
        destination_library_id: "01a0de1f-0000-7000-8000-000000000002",
        source_snapshot_sha256: "cd".repeat(32),
        source_snapshot_bytes: 10,
        source_through_server_seq: 1,
        notebook_count: 1,
        page_count: 1,
        blob_count: 0,
      },
    ],
    warnings: [],
    result: { outcome: "failed", error: "store_commit_uncertain", message: "The ledger refused or failed to publish the restore.", publication: "unknown", unindexed_blob_files_possible: true },
  };
  // What the server answers is scripted per step (the restore list only).
  let answer: { status: number; body: unknown } = { status: 200, body: { v: 1, jobs: [job] } };
  await page.route("**/control/v1/admin/restore/jobs", (r) => r.fulfill({ status: answer.status, contentType: "application/json", body: JSON.stringify(answer.body) }));

  await page.goto(product.url + "/settings/server");
  const card = page.getByTestId("restore-card");
  const row = card.getByTestId("restore-job");
  await expect(row).toHaveAttribute("data-state", "failed");
  await expect(row.getByTestId("restore-publication")).toHaveAttribute("data-publication", "unknown");
  await expect(row.getByTestId("restore-state")).toHaveText("Restore outcome uncertain");
  await expect(row).toContainText("Publication outcome could not be confirmed. Check the destination libraries before attempting another restore");
  await shot(page, "restore-unknown-desktop");

  // A transient failure keeps the last list.
  answer = { status: 503, body: { error: "unavailable", message: "Server busy." } };
  await card.getByTestId("restore-refresh").click();
  await expect(card.getByTestId("restore-error")).toContainText("Server busy.");
  await expect(row).toBeVisible();

  // A refused session (e.g. revoked) drops the list.
  answer = { status: 401, body: { error: "unauthorized", message: "not signed in" } };
  await card.getByTestId("restore-refresh").click();
  await expect(card.getByTestId("restore-error")).toHaveAttribute("data-kind", "session");
  await expect(card.getByTestId("restore-job")).toHaveCount(0);

  // The loopback rule (a LAN browser): explained, the rest of Settings unaffected.
  answer = { status: 403, body: { error: "loopback_required", message: "Backup restore is only available on the server itself (or an SSH tunnel to it)." } };
  await card.getByTestId("restore-refresh").click();
  await expect(card.getByTestId("restore-error")).toHaveAttribute("data-kind", "loopback");
  await expect(card.getByTestId("restore-error")).toContainText("opened on the server itself");
  await expect(card.getByTestId("restore-job")).toHaveCount(0);
  await expect(page.getByTestId("server-card")).toBeVisible();
  await shot(page, "restore-loopback-desktop");

  // Any other 403 is not mistaken for the loopback rule.
  answer = { status: 403, body: { error: "forbidden", message: "administrator access required" } };
  await card.getByTestId("restore-refresh").click();
  await expect(card.getByTestId("restore-error")).toHaveAttribute("data-kind", "access");
  await expect(card.getByTestId("restore-error")).toHaveText("administrator access required");
});

test("members and signed-out browsers never get the Backup & Restore section", async ({ browser, product }) => {
  const admin = await product.claimAsHarness();
  const member = await memberCookie(product, admin);
  const memberCtx = await browser.newContext();
  await memberCtx.addCookies([{ name: "scribe_session", value: member, url: product.url, httpOnly: true, sameSite: "Strict" }]);
  const m = await memberCtx.newPage();
  const seen = restoreTraffic(m);
  await m.goto(product.url + "/settings/server");
  await expect(m.getByTestId("library-chooser")).toBeVisible();
  await expect(m.getByTestId("settings")).toHaveCount(0);
  await expect(m.getByTestId("restore-card")).toHaveCount(0);
  // The server refuses a member regardless of the UI.
  expect((await product.call("GET", "/control/v1/admin/restore/jobs", { cookie: member })).status).toBe(403);

  const anonCtx = await browser.newContext();
  const a = await anonCtx.newPage();
  await a.goto(product.url + "/settings/server");
  await expect(a.getByTestId("request-heading")).toBeVisible();
  await expect(a.getByTestId("restore-card")).toHaveCount(0);
  expect(seen).toEqual([]);
  await memberCtx.close();
  await anonCtx.close();
});

devTest("developer token mode has no Server Settings and no restore section", async ({ page, server }) => {
  const seen = restoreTraffic(page);
  // Connected with the developer token (this server has no libraries: the chooser shows).
  await connect(page, server);
  await expect(page.getByTestId("library-chooser")).toBeVisible({ timeout: 20_000 });
  await page.goto(server.url + "/settings/server");
  await expect(page.getByTestId("library-chooser")).toBeVisible({ timeout: 20_000 });
  await expect(page.getByTestId("connect-form")).toHaveCount(0);
  await expect(page.getByTestId("settings")).toHaveCount(0);
  await expect(page.getByTestId("restore-card")).toHaveCount(0);
  expect(seen).toEqual([]);
});

// ------------------------------------------------------------------ Slice 1G-B: upload and cancel

const UPLOAD_PATH = "/control/v1/admin/restore/jobs";

/** Everything page script can reach: Web Storage, every IndexedDB store, Cache Storage. */
async function browserStorage(page: Page): Promise<string> {
  return page.evaluate(async () => {
    const out: unknown[] = [{ ...localStorage }, { ...sessionStorage }];
    const dbs = indexedDB.databases ? await indexedDB.databases() : [];
    for (const info of dbs) {
      if (!info.name) continue;
      const db = await new Promise<IDBDatabase>((ok, fail) => {
        const r = indexedDB.open(info.name!);
        r.onsuccess = () => ok(r.result);
        r.onerror = () => fail(r.error);
      });
      for (const store of Array.from(db.objectStoreNames)) {
        const rows = await new Promise<unknown[]>((ok) => {
          const q = db.transaction(store).objectStore(store).getAll();
          q.onsuccess = () => ok(q.result as unknown[]);
          q.onerror = () => ok([]);
        });
        out.push({ db: info.name, store, rows });
      }
      db.close();
    }
    if ("caches" in self) {
      for (const name of await caches.keys()) {
        const cache = await caches.open(name);
        for (const req of await cache.keys()) out.push(req.url, await (await cache.match(req))?.text());
      }
    }
    return JSON.stringify(out);
  });
}

async function openSettings(page: Page, product: ProductServer) {
  await page.goto(product.url + "/");
  await page.getByTestId("settings").click();
  await expect(page.getByTestId("restore-upload")).toBeVisible();
}

const zipInput = (bytes: Uint8Array, name = "ignite-backup.zip") => ({ name, mimeType: "application/zip", buffer: Buffer.from(bytes) });

test("an administrator uploads, previews and cancels a backup in the browser; nothing is restored", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  const libs = (await product.call("GET", "/v1/libraries", { bearer: kindle })).json as { libraries: { library_id: string; snapshot_sha256: string }[] };
  const original = libs.libraries[0]!;
  const zip = product.exportBackup(kindle);
  await asAdmin(context, product, admin);
  const traffic = restoreTraffic(page);
  const consoleLines: string[] = [];
  page.on("console", (m) => consoleLines.push(m.text()));

  await openSettings(page, product);
  const card = page.getByTestId("restore-card");
  await expect(card.getByTestId("restore-upload-button")).toBeDisabled();
  await card.getByTestId("restore-file").setInputFiles(zipInput(zip));
  await expect(card.getByTestId("restore-file-info")).toContainText("ignite-backup.zip");
  await shot(page, "upload-selected-desktop");

  const [req, resp] = await Promise.all([
    page.waitForRequest((r) => r.method() === "POST" && new URL(r.url()).pathname === UPLOAD_PATH),
    page.waitForResponse((r) => r.request().method() === "POST" && new URL(r.url()).pathname === UPLOAD_PATH),
    card.getByTestId("restore-upload-button").click(),
  ]);
  const headers = await req.allHeaders();
  expect(headers["content-type"]).toBe("application/zip");
  expect(headers["x-scribe-request"]).toBe("1");
  expect(headers["authorization"]).toBeUndefined();
  expect(headers["cookie"] ?? "").toContain("scribe_session="); // where the browser reports it
  expect(resp.status()).toBe(201);
  const answer = (await resp.json()) as Prepared;
  const secret = answer.confirmation;
  expect(secret).toMatch(/^[A-Za-z0-9_-]{43}$/);

  // The verified preview, from the server's answer.
  const prepared = card.getByTestId("restore-prepared");
  await expect(prepared).toContainText("Verification complete");
  await expect(prepared).toContainText("This is a preview. No notebooks have been restored.");
  const row = card.getByTestId("restore-job");
  await expect(row).toHaveAttribute("data-state", "ready");
  await expect(row.getByTestId("restore-details")).toBeVisible(); // opened for review
  const lib = answer.libraries[0]!;
  expect(lib.source_library_id).toBe(original.library_id);
  await expect(row.getByTestId("restore-source")).toHaveText(lib.source_library_id);
  await expect(row.getByTestId("restore-destination")).toHaveText(lib.destination_library_id);
  await page.setViewportSize({ width: 1280, height: 900 });
  await shot(page, "upload-prepared-desktop");
  await page.setViewportSize({ width: 390, height: 844 });
  await shot(page, "upload-prepared-mobile");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.setViewportSize({ width: 1280, height: 900 });

  // The one-time confirmation is nowhere the page exposes it.
  expect(await page.content()).not.toContain(secret);
  expect(page.url()).not.toContain(secret);
  expect(consoleLines.join("\n")).not.toContain(secret);
  expect(await browserStorage(page)).not.toContain(secret);
  expect(JSON.stringify(await context.cookies())).not.toContain(secret);

  // Nothing was restored.
  const ids = ((await product.call("GET", "/v1/libraries", { bearer: kindle })).json as { libraries: { library_id: string }[] }).libraries.map((l) => l.library_id);
  expect(ids).toEqual([original.library_id]);

  // Cancel through the browser (asked first).
  await row.getByTestId("restore-cancel").click();
  await expect(row.getByTestId("restore-cancel-confirm")).toContainText("Discard this prepared restore?");
  await shot(page, "cancel-confirm-desktop");
  const [del] = await Promise.all([
    page.waitForRequest((r) => r.method() === "DELETE"),
    row.getByTestId("restore-cancel-yes").click(),
  ]);
  expect(new URL(del.url()).pathname).toBe(`${UPLOAD_PATH}/${answer.job_id}`);
  expect((await del.allHeaders())["x-scribe-request"]).toBe("1");
  await expect(card.getByTestId("restore-cancel-notice")).toHaveAttribute("data-tone", "ok");
  await expect(card.getByTestId("restore-job")).toHaveCount(0);
  await expect(card.getByTestId("restore-prepared")).toHaveCount(0);
  const left = (await product.call("GET", UPLOAD_PATH, { cookie: admin })).json as { jobs: unknown[] };
  expect(left.jobs).toEqual([]); // withdrawn and its staging deleted
  expect((await product.call("GET", `${UPLOAD_PATH}/${answer.job_id}`, { cookie: admin })).status).toBe(404);

  // The source library, its snapshot and its blob are unchanged.
  const after = ((await product.call("GET", "/v1/libraries", { bearer: kindle })).json as { libraries: { library_id: string; snapshot_sha256: string }[] }).libraries;
  expect(after).toEqual(libs.libraries);
  expect(after[0]!.snapshot_sha256).toBe(original.snapshot_sha256);

  // The browser used only GET, the upload POST and the DELETE: never the confirmation route.
  expect(traffic.filter((r) => r.url().includes("/confirm"))).toEqual([]);
  expect(new Set(traffic.map((r) => r.method()))).toEqual(new Set(["GET", "POST", "DELETE"]));
  for (const r of traffic) expect(await r.headerValue("authorization")).toBeNull();
});

test("an invalid backup and a full server are refused with explanations", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  const card = page.getByTestId("restore-card");

  // Not a ZIP: the server's verification refuses it, and no job exists.
  await card.getByTestId("restore-file").setInputFiles(zipInput(new TextEncoder().encode("this is not a zip archive"), "broken.zip"));
  await card.getByTestId("restore-upload-button").click();
  const error = card.getByTestId("restore-upload-error");
  await expect(error).toHaveAttribute("data-kind", "rejected");
  await expect(error).toContainText("Backup could not be verified");
  await expect(error).toContainText("archive_invalid: zip_not_zip");
  await shot(page, "upload-rejected-desktop");
  expect(((await product.call("GET", UPLOAD_PATH, { cookie: admin })).json as { jobs: unknown[] }).jobs).toEqual([]);

  // A prepared restore of this administrator already exists (made by the harness): refused.
  const zip = product.exportBackup(kindle);
  const existing = await product.call("POST", UPLOAD_PATH, { cookie: admin, body: zip, contentType: "application/zip" });
  expect(existing.status).toBe(201);
  await card.getByTestId("restore-file").setInputFiles(zipInput(zip));
  await card.getByTestId("restore-upload-button").click();
  await expect(error).toHaveAttribute("data-kind", "capacity");
  await expect(error).toContainText("one prepared restore per administrator");
  // The existing job was not touched to make room.
  const jobs = ((await product.call("GET", UPLOAD_PATH, { cookie: admin })).json as { jobs: { job_id: string; state: string }[] }).jobs;
  expect(jobs.map((j) => [j.job_id, j.state])).toEqual([[(existing.json as Prepared).job_id, "ready"]]);
});

test("a lost upload response is reported as uncertain; the prepared job can be found and cancelled", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  const zip = product.exportBackup(kindle);
  await asAdmin(context, product, admin);
  // The server receives the upload and answers 201, but the browser never gets the answer.
  let delivered: Prepared | null = null;
  let status = 0;
  await page.route(`**${UPLOAD_PATH}`, async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    // Forward the selected bytes explicitly: Playwright's WebKit interception does not expose a
    // File request body (it would forward 0 bytes). Uninterrupted uploads send the File itself.
    const real = await route.fetch({ postData: Buffer.from(zip) });
    status = real.status();
    delivered = (await real.json()) as Prepared;
    await route.abort("connectionreset");
  });
  await openSettings(page, product);
  const card = page.getByTestId("restore-card");
  await card.getByTestId("restore-file").setInputFiles(zipInput(zip));
  await card.getByTestId("restore-upload-button").click();
  const error = card.getByTestId("restore-upload-error");
  await expect(error).toHaveAttribute("data-kind", "uncertain");
  await expect(error).toContainText("Refresh the list before uploading again");
  await shot(page, "upload-uncertain-desktop");
  expect(status, JSON.stringify(delivered)).toBe(201); // the server did prepare it
  // The list refreshes on its own after an uncertain outcome; the job is there to cancel.
  const row = card.getByTestId("restore-job");
  await expect(row).toHaveAttribute("data-state", "ready");
  await expect(card.getByTestId("restore-upload-button")).toBeDisabled(); // not retried, no file selected
  await row.getByTestId("restore-cancel").click();
  await row.getByTestId("restore-cancel-yes").click();
  await expect(card.getByTestId("restore-job")).toHaveCount(0);
  expect(((await product.call("GET", UPLOAD_PATH, { cookie: admin })).json as { jobs: unknown[] }).jobs).toEqual([]);
  expect(await page.content()).not.toContain(delivered!.confirmation);
});

// ------------------------------------------------------------------ Slice 1G-C: confirmed restore

interface Lib {
  library_id: string;
  head_server_seq: number;
  snapshot_sha256: string;
  notebook_count: number;
  page_count: number;
}

/** A second library: the fixture snapshot under another valid id (the id occurs once in it). */
const SECOND_LIBRARY = "01a14ab8-8368-75bf-ae2e-f7fcd3c81bcd";

async function libraries(product: ProductServer, bearer: string): Promise<Lib[]> {
  return ((await product.call("GET", "/v1/libraries", { bearer })).json as { libraries: Lib[] }).libraries;
}

/** Two libraries (sharing the fixture background blob), as a Kindle would have synced them. */
async function seedTwo(product: ProductServer, bearer: string): Promise<void> {
  await product.seedFixture(bearer);
  const second = snapshotText.replace(manifest.library_id, SECOND_LIBRARY);
  expect((await product.call("POST", "/v1/libraries", { bearer, body: second, csrf: false })).status).toBe(201);
}

/** Upload through the browser's file input; returns the server's 201 (held by the test only). */
async function uploadInBrowser(page: Page, zip: Uint8Array): Promise<Prepared> {
  const card = page.getByTestId("restore-card");
  await card.getByTestId("restore-file").setInputFiles(zipInput(zip));
  const [resp] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === "POST" && new URL(r.url()).pathname === UPLOAD_PATH),
    card.getByTestId("restore-upload-button").click(),
  ]);
  expect(resp.status()).toBe(201);
  await expect(card.getByTestId("restore-prepared")).toBeVisible();
  return (await resp.json()) as Prepared;
}

const confirmPath = (id: string) => `${UPLOAD_PATH}/${id}/confirm`;
const isConfirm = (r: Request) => r.method() === "POST" && new URL(r.url()).pathname.endsWith("/confirm");

/** Open the review, acknowledge, and click the final action. */
async function approve(page: Page) {
  const row = page.getByTestId("restore-job");
  await row.getByTestId("restore-review").click();
  await row.getByTestId("restore-ack").check();
  await row.getByTestId("restore-confirm-yes").click();
}

/** Show a job's details (they may already be open: a fresh upload opens them). */
async function openDetails(row: ReturnType<Page["getByTestId"]>) {
  const toggle = row.getByTestId("restore-toggle");
  if ((await toggle.getAttribute("aria-expanded")) !== "true") await toggle.click();
}

async function finalState(page: Page): Promise<string | null> {
  const row = page.getByTestId("restore-job");
  await expect
    .poll(async () => {
      const s = await row.getAttribute("data-state");
      if (s !== "succeeded" && s !== "failed") await page.getByTestId("restore-refresh").click();
      return s;
    }, { timeout: 20_000 })
    .toMatch(/^(succeeded|failed)$/);
  return row.getAttribute("data-state");
}

/** Everything a secret could leak into, checked without ever printing it. */
async function exposesSecret(page: Page, context: BrowserContext, secret: string, logs: string[]): Promise<boolean> {
  return (
    (await page.content()).includes(secret) ||
    page.url().includes(secret) ||
    logs.join("\n").includes(secret) ||
    (await browserStorage(page)).includes(secret) ||
    JSON.stringify(await context.cookies()).includes(secret)
  );
}

test("an administrator restores a verified two-library backup in the browser; existing libraries are untouched", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await seedTwo(product, kindle);
  const before = await libraries(product, kindle);
  expect(before).toHaveLength(2);
  const snapshotsBefore = await Promise.all(before.map(async (l) => (await product.call("GET", `/v1/libraries/${l.library_id}/snapshot`, { bearer: kindle })).json));
  const zip = product.exportBackup(kindle);
  await asAdmin(context, product, admin);
  const logs: string[] = [];
  page.on("console", (m) => logs.push(m.text()));
  const confirmRequests: Request[] = [];
  page.on("request", (r) => {
    if (isConfirm(r)) confirmRequests.push(r);
  });

  await openSettings(page, product);
  const prepared = await uploadInBrowser(page, zip);
  const secret = prepared.confirmation;
  expect(prepared.libraries).toHaveLength(2);
  const approved = new Map(prepared.libraries.map((l) => [l.source_library_id, l.destination_library_id]));
  expect([...approved.keys()].sort()).toEqual(before.map((l) => l.library_id).sort());
  const ids = async () => (await libraries(product, kindle)).map((l) => l.library_id);
  for (const d of approved.values()) expect(await ids()).not.toContain(d); // nothing restored yet

  const row = page.getByTestId("restore-job");
  await expect(row.getByTestId("restore-ready-note")).toContainText("Verified and ready");
  // The review sends nothing; backing out sends nothing; the final action needs the acknowledgement.
  await row.getByTestId("restore-review").click();
  const panel = row.getByTestId("restore-confirm-panel");
  await expect(panel.getByRole("heading", { name: "Confirm restoration" })).toBeVisible();
  await expect(panel).toContainText("Ignite will create 2 new libraries");
  await expect(panel).toContainText("Your existing libraries will not be replaced, merged or deleted.");
  for (const [src, dst] of approved) {
    await expect(panel.getByTestId("restore-source").filter({ hasText: src })).toHaveCount(1);
    await expect(panel.getByTestId("restore-destination").filter({ hasText: dst })).toHaveCount(1);
  }
  await expect(panel.getByTestId("restore-confirm-yes")).toBeDisabled();
  await expect(row.getByTestId("restore-cancel")).toHaveCount(0);
  await page.setViewportSize({ width: 1280, height: 900 });
  await shot(page, "confirm-review-desktop");
  await page.setViewportSize({ width: 390, height: 844 });
  await shot(page, "confirm-review-mobile");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.setViewportSize({ width: 1280, height: 900 });
  await panel.getByTestId("restore-confirm-back").click();
  await expect(row.getByTestId("restore-confirm-panel")).toHaveCount(0);
  expect(confirmRequests).toHaveLength(0);

  // The final, acknowledged action: exactly one POST, carrying exactly the reviewed plan.
  await row.getByTestId("restore-review").click();
  await row.getByTestId("restore-ack").check();
  const [req, resp] = await Promise.all([
    page.waitForRequest(isConfirm),
    page.waitForResponse((r) => isConfirm(r.request())),
    row.getByTestId("restore-confirm-yes").click(),
  ]);
  expect(new URL(req.url()).pathname).toBe(confirmPath(prepared.job_id));
  const headers = await req.allHeaders();
  expect([headers["content-type"], headers["x-scribe-request"], headers["authorization"]]).toEqual(["application/json", "1", undefined]);
  expect((headers["cookie"] ?? "").includes("scribe_session=")).toBe(true);
  const body = req.postDataJSON() as { v: number; confirmation: string; archive_sha256: string; destinations: { source_library_id: string; destination_library_id: string }[] };
  expect(body.v).toBe(1);
  expect(body.confirmation === secret).toBe(true); // compared, never printed
  expect(body.archive_sha256).toBe(prepared.archive.sha256);
  expect(new Map(body.destinations.map((d) => [d.source_library_id, d.destination_library_id]))).toEqual(approved);
  expect(resp.status()).toBe(202);
  expect(await finalState(page)).toBe("succeeded");
  expect(confirmRequests).toHaveLength(1);
  await expect(row.getByTestId("restore-publication")).toHaveAttribute("data-publication", "complete");
  await expect(row).toContainText("2 libraries restored as new libraries.");
  await expect(row).toContainText("Existing libraries were not replaced.");
  await openDetails(row);
  for (const dst of approved.values()) await expect(row.getByTestId("restore-destination").filter({ hasText: dst })).toHaveCount(1);
  await expect(row.getByTestId("restore-restored-sha")).toHaveCount(2);
  await shot(page, "confirm-succeeded-desktop");
  await page.setViewportSize({ width: 390, height: 844 });
  await shot(page, "confirm-succeeded-mobile");
  await page.setViewportSize({ width: 1280, height: 900 });

  // The restored data, through the server's real read and sync APIs.
  const after = await libraries(product, kindle);
  expect(after).toHaveLength(4);
  for (const b of before) expect(after.find((l) => l.library_id === b.library_id)).toEqual(b); // unchanged
  for (const [i, b] of before.entries()) {
    expect((await product.call("GET", `/v1/libraries/${b.library_id}/snapshot`, { bearer: kindle })).json).toEqual(snapshotsBefore[i]);
  }
  const blob = await product.call("GET", `/v1/blobs/${manifest.blob.sha256}`, { bearer: kindle });
  expect(blob.status).toBe(200);
  for (const [src, dst] of approved) {
    const source = (snapshotsBefore[before.findIndex((l) => l.library_id === src)] as { snapshot: Record<string, unknown> }).snapshot;
    const lib = after.find((l) => l.library_id === dst)!;
    const original = before.find((l) => l.library_id === src)!;
    expect([lib.head_server_seq, lib.notebook_count, lib.page_count]).toEqual([0, original.notebook_count, original.page_count]);
    const genesis = (await product.call("GET", `/v1/libraries/${dst}/genesis`, { bearer: kindle })).json as { genesis_sha256: string; snapshot: Record<string, unknown> };
    expect(genesis.snapshot).toEqual({ ...source, library_id: dst }); // the archive's state, re-keyed only
    const current = (await product.call("GET", `/v1/libraries/${dst}/snapshot`, { bearer: kindle })).json as { through_server_seq: number; snapshot_sha256: string };
    expect([current.through_server_seq, current.snapshot_sha256]).toEqual([0, genesis.genesis_sha256]); // replay = genesis
    const ops = (await product.call("GET", `/v1/libraries/${dst}/operations?after=0&limit=500`, { bearer: kindle })).json as { operations: unknown[] };
    expect(ops.operations).toEqual([]);
  }

  // Never twice: the same capability is refused by the server, nothing more is created.
  const replay = await product.call("POST", confirmPath(prepared.job_id), { cookie: admin, body });
  expect(replay.status).toBe(409);
  expect(await libraries(product, kindle)).toHaveLength(4);
  expect(await exposesSecret(page, context, secret, logs)).toBe(false);

  // A reload still shows the retained result (read from the server).
  await page.reload();
  await expect(page.getByTestId("restore-job")).toHaveAttribute("data-state", "succeeded");
  await expect(page.getByTestId("restore-publication")).toHaveAttribute("data-publication", "complete");
  await expect(page.getByTestId("restore-review")).toHaveCount(0);
});

test("after a reload the confirmation is gone: the job can be reviewed or cancelled, not restored", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  await uploadInBrowser(page, product.exportBackup(kindle));
  await expect(page.getByTestId("restore-review")).toBeVisible();
  await page.reload();
  const row = page.getByTestId("restore-job");
  await expect(row).toHaveAttribute("data-state", "ready");
  await expect(row.getByTestId("restore-ready-note")).toContainText("this browser no longer has its one-time confirmation");
  await expect(row.getByTestId("restore-review")).toHaveCount(0);
  await expect(row.getByTestId("restore-cancel")).toBeVisible();
  await shot(page, "confirm-lost-capability-desktop");
});

test("busy, already-confirmed and refused confirmations are explained; busy keeps the job confirmable", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  const zip = product.exportBackup(kindle);
  const row = page.getByTestId("restore-job");

  // 429: the server is busy (simulated without reaching it): nothing started, still confirmable.
  let prepared = await uploadInBrowser(page, zip);
  await page.route(`**${confirmPath(prepared.job_id)}`, (r) =>
    r.fulfill({ status: 429, contentType: "application/json", body: JSON.stringify({ error: "restore_busy", message: "Another restore is running." }) }),
  );
  await approve(page);
  await expect(row.getByTestId("restore-confirm-notice")).toHaveAttribute("data-kind", "refused-capacity");
  await expect(row.getByTestId("restore-review")).toBeVisible();
  await page.unroute(`**${confirmPath(prepared.job_id)}`);
  // The kept capability then works with the real server.
  await approve(page);
  expect(await finalState(page)).toBe("succeeded");
  await expect(page.getByTestId("restore-cancel-notice")).toHaveCount(0);

  // 409: confirmed elsewhere first (the harness holds the same capability).
  await page.getByTestId("restore-file").setInputFiles(zipInput(zip));
  const [resp] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === "POST" && new URL(r.url()).pathname === UPLOAD_PATH),
    page.getByTestId("restore-upload-button").click(),
  ]);
  prepared = (await resp.json()) as Prepared;
  const fresh = page.locator(`[data-testid="restore-job"][data-job-id="${prepared.job_id}"]`);
  const first = await product.call("POST", confirmPath(prepared.job_id), {
    cookie: admin,
    body: {
      v: 1,
      confirmation: prepared.confirmation,
      archive_sha256: prepared.archive.sha256,
      destinations: prepared.libraries.map((l) => ({ source_library_id: l.source_library_id, destination_library_id: l.destination_library_id })),
    },
  });
  expect(first.status).toBe(202);
  await fresh.getByTestId("restore-review").click();
  await fresh.getByTestId("restore-ack").check();
  await fresh.getByTestId("restore-confirm-yes").click();
  const refused = page.getByTestId("restore-confirm-notice").filter({ hasText: "Already confirmed" });
  await expect(refused).toBeVisible();
  await expect(page.getByTestId("restore-review")).toHaveCount(0);
  await shot(page, "confirm-already-confirmed-desktop");
});

test("a lost 202 is reported as uncertain, never resent, and settled by the server's status", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  const prepared = await uploadInBrowser(page, product.exportBackup(kindle));
  const sent: Request[] = [];
  page.on("request", (r) => {
    if (isConfirm(r)) sent.push(r);
  });
  let accepted = 0;
  // The server receives the confirmation and accepts it; the browser never gets the answer.
  await page.route(`**${confirmPath(prepared.job_id)}`, async (route) => {
    const real = await route.fetch();
    accepted = real.status();
    await route.abort("connectionreset");
  });
  await approve(page);
  const row = page.getByTestId("restore-job");
  await expect(page.getByText("Confirmation outcome unknown. Checking restore status.").or(row.getByTestId("restore-publication"))).toBeVisible();
  await shot(page, "confirm-uncertain-desktop");
  expect(accepted).toBe(202);
  expect(await finalState(page)).toBe("succeeded");
  await expect(row.getByTestId("restore-publication")).toHaveAttribute("data-publication", "complete");
  expect(sent).toHaveLength(1); // never resent
  await expect(page.getByTestId("restore-prepared")).toHaveCount(0); // no stale "nothing restored" preview
  expect((await libraries(product, kindle)).length).toBe(2); // restored exactly once
  await expect(row.getByTestId("restore-review")).toHaveCount(0);
});

test("a real terminal failure publishes nothing and says so", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  const prepared = await uploadInBrowser(page, product.exportBackup(kindle));
  // A library appears under the proposed destination identity before the restore runs.
  const dst = prepared.libraries[0]!.destination_library_id;
  const squatter = snapshotText.replace(manifest.library_id, dst);
  expect((await product.call("POST", "/v1/libraries", { bearer: kindle, body: squatter, csrf: false })).status).toBe(201);
  const before = await libraries(product, kindle);
  await approve(page);
  expect(await finalState(page)).toBe("failed");
  const row = page.getByTestId("restore-job");
  await expect(row.getByTestId("restore-publication")).toHaveAttribute("data-publication", "none");
  await expect(row).toContainText("No libraries were published by this restore.");
  await expect(row.getByTestId("restore-state")).toHaveText("Restoration failed");
  await shot(page, "confirm-failed-desktop");
  expect(await libraries(product, kindle)).toEqual(before); // nothing published or replaced
});

test("an access refusal on confirmation drops the job list at once", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  const prepared = await uploadInBrowser(page, product.exportBackup(kindle));
  await page.route(`**${confirmPath(prepared.job_id)}`, (r) =>
    r.fulfill({ status: 401, contentType: "application/json", body: JSON.stringify({ error: "unauthorized", message: "not signed in" }) }),
  );
  await approve(page);
  const card = page.getByTestId("restore-card");
  await expect(card.getByTestId("restore-error")).toHaveAttribute("data-kind", "session");
  await expect(card.getByTestId("restore-job")).toHaveCount(0);
  await expect(card.getByTestId("restore-upload")).toHaveCount(0);
  // Nothing was confirmed on the server.
  const jobs = ((await product.call("GET", UPLOAD_PATH, { cookie: admin })).json as { jobs: { state: string }[] }).jobs;
  expect(jobs.map((j) => j.state)).toEqual(["ready"]);
});

test("an unanswered confirmation stays uncertain while status checks fail, until the server answers", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  const prepared = await uploadInBrowser(page, product.exportBackup(kindle)); // routes added only after the upload
  const sent: Request[] = [];
  page.on("request", (r) => {
    if (isConfirm(r)) sent.push(r);
  });
  // After the confirmation, every status GET fails until released.
  let failLists = false;
  await page.route(`**${UPLOAD_PATH}`, (r) => (r.request().method() === "GET" && failLists ? r.abort("connectionreset") : r.continue()));
  let accepted = 0;
  await page.route(`**${confirmPath(prepared.job_id)}`, async (route) => {
    failLists = true;
    const real = await route.fetch(); // the server accepts it...
    accepted = real.status();
    await route.abort("connectionreset"); // ...but the browser never hears back
  });
  await approve(page);
  const card = page.getByTestId("restore-card");
  const row = card.getByTestId("restore-job");
  await expect(row.getByTestId("restore-confirm-notice")).toHaveAttribute("data-kind", "uncertain");
  await expect(card.getByTestId("restore-error")).toHaveAttribute("data-kind", "transient"); // the status check failed
  expect(accepted).toBe(202);
  // Still uncertain: the old "ready" row is shown as data, but nothing claims it as the status.
  const checkUncertain = async () => {
    await expect(row).toHaveAttribute("data-state", "ready");
    await expect(row.getByTestId("restore-state")).toHaveText("Checking server status");
    await expect(row.getByTestId("restore-confirm-notice")).toHaveAttribute("data-kind", "uncertain");
    await expect(card.getByTestId("restore-prepared")).toHaveCount(0); // no "nothing restored" preview
    await expect(card).not.toContainText("No notebooks have been restored");
    await expect(card).not.toContainText("Verified, awaiting confirmation");
    await expect(card).not.toContainText("has not started");
    await expect(row.getByTestId("restore-review")).toHaveCount(0);
    await expect(row.getByTestId("restore-cancel")).toHaveCount(0);
  };
  await checkUncertain();
  await shot(page, "confirm-uncertain-status-failing-desktop");
  // Another failing check changes nothing.
  const [failed] = await Promise.all([page.waitForEvent("requestfailed", (r) => new URL(r.url()).pathname === UPLOAD_PATH), card.getByTestId("restore-refresh").click()]);
  expect(failed.method()).toBe("GET");
  await checkUncertain();
  // The server answers: the restore had been accepted, and its actual result is shown.
  failLists = false;
  expect(await finalState(page)).toBe("succeeded");
  await expect(row.getByTestId("restore-publication")).toHaveAttribute("data-publication", "complete");
  expect(sent).toHaveLength(1); // never resent
  expect((await libraries(product, kindle)).length).toBe(2);
});
