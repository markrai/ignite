// Slice 1G-D2: Server Settings → Backup & Restore → Backup, against the real `scribe-server`.
// The browser asks the server to prepare a verified backup, downloads it natively (the file the
// browser saved is what is checked), and discards the server's temporary copy. The archive is
// checked independently: its length and SHA-256 here, and its contents by the server's strict
// restore verifier (`backup_archive::inspect_backup`, through the restore upload).

import { createHash, randomBytes } from "node:crypto";
import { readFileSync, statSync } from "node:fs";
import type { Download, Page, Request, Route } from "@playwright/test";
import { manifest, snapshotText } from "./harness";
import { asAdmin, expect, test, type ProductServer } from "./product";

const EXPORTS = "/control/v1/admin/backup/exports";
const RESTORE_JOBS = "/control/v1/admin/restore/jobs";
/** A second library: the fixture snapshot under another valid id (the id occurs once in it). */
const SECOND_LIBRARY = "01a14ab8-8368-75bf-ae2e-f7fcd3c81bcd";
const LARGE_LIBRARY = "01a14ab8-8368-75bf-ae2e-f7fcd3c81bce";

interface Lib {
  library_id: string;
  head_server_seq: number;
  snapshot_sha256: string;
  notebook_count: number;
  page_count: number;
}

interface Summary {
  export_id: string;
  state: string;
  expires_in_s: number | null;
  archive: { bytes: number; sha256: string; file_name: string } | null;
  totals: { libraries: number; notebooks: number; pages: number; blobs: number } | null;
  download: string | null;
}

async function libraries(product: ProductServer, bearer: string): Promise<Lib[]> {
  return ((await product.call("GET", "/v1/libraries", { bearer })).json as { libraries: Lib[] }).libraries;
}

/** Every library's list row and current snapshot (to prove nothing changed). */
async function ledger(product: ProductServer, bearer: string): Promise<unknown> {
  const libs = await libraries(product, bearer);
  return Promise.all(libs.map(async (l) => [l, (await product.call("GET", `/v1/libraries/${l.library_id}/snapshot`, { bearer })).json]));
}

/** Two libraries sharing the fixture background blob, as a Kindle would have synced them. */
async function seedTwo(product: ProductServer, bearer: string): Promise<void> {
  await product.seedFixture(bearer);
  const second = snapshotText.replace(manifest.library_id, SECOND_LIBRARY);
  expect((await product.call("POST", "/v1/libraries", { bearer, body: second, csrf: false })).status).toBe(201);
}

/**
 * A library whose pages use a large, incompressible background (24 MiB): preparing its backup
 * takes the server a few seconds, long enough to observe (and reload during) preparation.
 */
async function seedLarge(product: ProductServer, bearer: string): Promise<void> {
  const [w, h] = [4096, 6144];
  const blob = randomBytes(w * h);
  const sha = createHash("sha256").update(blob).digest("hex");
  expect((await product.call("PUT", `/v1/blobs/${sha}`, { bearer, body: new Uint8Array(blob), csrf: false })).status).toBeLessThan(300);
  const ref = /"sha256":"43c443ca[0-9a-f]+","byte_len":\d+,"format":\{"encoding":"gray8","width":\d+,"height":\d+,"stride":\d+\}/g;
  const text = snapshotText
    .replace(manifest.library_id, LARGE_LIBRARY)
    .replace(ref, `"sha256":"${sha}","byte_len":${blob.length},"format":{"encoding":"gray8","width":${w},"height":${h},"stride":${w}}`);
  expect(text).toContain(sha);
  expect((await product.call("POST", "/v1/libraries", { bearer, body: text, csrf: false })).status).toBe(201);
}

async function openSettings(page: Page, product: ProductServer) {
  await page.goto(product.url + "/");
  await page.getByTestId("settings").click();
  await expect(page.getByTestId("backup-section")).toBeVisible();
  await expect(page.getByTestId("backup-create")).toBeEnabled();
}

/** Every request the page makes to the export routes. */
function exportTraffic(page: Page): Request[] {
  const seen: Request[] = [];
  page.on("request", (r) => {
    if (new URL(r.url()).pathname.startsWith(EXPORTS)) seen.push(r);
  });
  return seen;
}

const isStart = (r: Request) => r.method() === "POST" && new URL(r.url()).pathname === EXPORTS;
const isList = (r: Request) => r.method() === "GET" && new URL(r.url()).pathname === EXPORTS;

/** Optional screenshots of the section (Settings scrolls inside its own container). */
async function shot(page: Page, name: string) {
  const dir = process.env.RESTORE_SCREENSHOT_DIR;
  if (dir) await page.getByTestId("restore-card").screenshot({ path: `${dir}/${name}.png` });
}

async function ownExports(product: ProductServer, cookie: string): Promise<Summary[]> {
  return ((await product.call("GET", EXPORTS, { cookie })).json as { exports: Summary[] }).exports;
}

/** Click Download and save the file the browser received. */
async function downloadInBrowser(page: Page, path: string): Promise<Download> {
  const [download] = await Promise.all([page.waitForEvent("download"), page.getByTestId("backup-download").click()]);
  expect(await download.failure()).toBeNull();
  await download.saveAs(path);
  return download;
}

const sha256 = (path: string) => createHash("sha256").update(readFileSync(path)).digest("hex");

/** The server's strict verifier, through the restore upload (the prepared job is discarded again). */
async function verifyWithServer(product: ProductServer, admin: string, zip: Uint8Array) {
  const up = await product.call("POST", RESTORE_JOBS, { cookie: admin, body: zip, contentType: "application/zip" });
  expect(up.status, JSON.stringify(up.json)).toBe(201);
  const plan = up.json as { job_id: string; archive: { sha256: string; bytes: number }; totals: { libraries: number; blobs: number }; libraries: { source_library_id: string }[] };
  expect((await product.call("DELETE", `${RESTORE_JOBS}/${plan.job_id}`, { cookie: admin })).status).toBe(200);
  return plan;
}

test("an administrator creates, downloads (natively) and discards a verified backup of two libraries", async ({ page, context, product }, testInfo) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await seedTwo(product, kindle);
  const before = await ledger(product, kindle);
  const libs = await libraries(product, kindle);
  await asAdmin(context, product, admin);
  const traffic = exportTraffic(page);
  const logs: string[] = [];
  page.on("console", (m) => logs.push(m.text()));
  page.on("pageerror", (e) => logs.push(`pageerror: ${e.message}`));

  await openSettings(page, product);
  const section = page.getByTestId("backup-section");
  await expect(section.getByTestId("backup-empty")).toBeVisible();
  await expect(page.getByTestId("restore-upload")).toBeVisible(); // Restore is still there, below

  // Hold the server's first answers about the new export, so "preparing" is observed as such.
  let release!: () => void;
  const held = new Promise<void>((ok) => (release = ok));
  await page.route(`**${EXPORTS}`, async (route: Route) => {
    if (route.request().method() === "GET") await held;
    await route.continue();
  });
  const [start] = await Promise.all([page.waitForResponse((r) => isStart(r.request())), section.getByTestId("backup-create").click()]);
  expect(start.status()).toBe(202);
  const accepted = (await start.json()) as Summary;
  expect(accepted.state).toBe("preparing");
  const row = section.getByTestId("backup-export");
  await expect(row).toHaveAttribute("data-state", "preparing");
  await expect(row.getByTestId("backup-preparing")).toContainText("Preparing and verifying your backup…");
  await expect(row.getByTestId("backup-download")).toHaveCount(0);
  await expect(section.getByTestId("backup-create")).toBeDisabled();
  release(); // from now on every request passes straight through

  // The server's own report: ready, with the real counts.
  await expect(row).toHaveAttribute("data-state", "ready", { timeout: 20_000 });
  const ready = (await ownExports(product, admin))[0]!;
  expect(ready.export_id).toBe(accepted.export_id);
  await expect(row.getByTestId("backup-libraries")).toHaveText("2");
  await expect(row.getByTestId("backup-notebooks")).toHaveText(String(libs.reduce((n, l) => n + l.notebook_count, 0)));
  await expect(row.getByTestId("backup-pages")).toHaveText(String(libs.reduce((n, l) => n + l.page_count, 0)));
  await expect(row.getByTestId("backup-blobs")).toContainText("1 ("); // the shared background, once
  await expect(row.getByTestId("backup-sha")).toHaveText(ready.archive!.sha256);
  await expect(row).toContainText(`${ready.archive!.bytes.toLocaleString("en-US")} bytes`);
  await expect(row.getByTestId("backup-consistency")).toContainText("not one atomic snapshot across all libraries");
  await expect(row.getByTestId("backup-download")).toHaveAttribute("href", `${EXPORTS}/${ready.export_id}/download`);
  await page.setViewportSize({ width: 1280, height: 900 });
  await shot(page, "backup-ready-desktop");
  await page.setViewportSize({ width: 390, height: 844 });
  await shot(page, "backup-ready-mobile");
  // The checksum and ids wrap: nothing scrolls the page sideways on a phone.
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.setViewportSize({ width: 1280, height: 900 });

  // Native download: the browser streams the file; the page never fetches it.
  const path = testInfo.outputPath("ignite-backup.zip");
  const download = await downloadInBrowser(page, path);
  expect(download.suggestedFilename()).toBe("ignite-backup.zip");
  expect(statSync(path).size).toBe(ready.archive!.bytes);
  expect(sha256(path)).toBe(ready.archive!.sha256);
  await expect(row.getByTestId("backup-download-started")).toContainText("Download started in your browser");
  // The browser fetched it as a download of exactly this same-origin route; the page script
  // never requested it (no fetch/XHR to the download route at all).
  expect(download.url()).toBe(`${product.url}${EXPORTS}/${ready.export_id}/download`);
  const scripted = traffic.filter((r) => new URL(r.url()).pathname.endsWith("/download") && ["fetch", "xhr"].includes(r.resourceType()));
  expect(scripted).toEqual([]);

  // The strict verifier accepts the saved file: both source libraries, one shared blob.
  const plan = await verifyWithServer(product, admin, new Uint8Array(readFileSync(path)));
  expect(plan.archive).toMatchObject({ sha256: ready.archive!.sha256, bytes: ready.archive!.bytes });
  expect(plan.libraries.map((l) => l.source_library_id).sort()).toEqual([manifest.library_id, SECOND_LIBRARY].sort());
  expect(plan.totals).toMatchObject({ libraries: 2, blobs: 1 });

  // Still available after the download, and downloadable again.
  await expect(row).toHaveAttribute("data-state", "ready");
  const again = testInfo.outputPath("again.zip");
  await downloadInBrowser(page, again);
  expect(sha256(again)).toBe(ready.archive!.sha256);

  // Discard: an explicit question first, then the server's temporary copy is gone.
  await row.getByTestId("backup-discard").click();
  await expect(row.getByTestId("backup-discard-confirm")).toContainText("does not delete your notebooks");
  await shot(page, "backup-discard-desktop");
  await row.getByTestId("backup-discard-yes").click();
  await expect(section.getByTestId("backup-export")).toHaveCount(0);
  await expect(section.getByTestId("backup-discard-notice")).toHaveAttribute("data-tone", "ok");
  expect(await ownExports(product, admin)).toEqual([]);
  expect((await product.call("GET", `${EXPORTS}/${ready.export_id}/download`, { cookie: admin })).status).toBe(404);
  await expect(section.getByTestId("backup-create")).toBeEnabled();

  // The source libraries are exactly as before; the browser only used its session cookie.
  expect(await ledger(product, kindle)).toEqual(before);
  const starts = traffic.filter(isStart);
  expect(starts).toHaveLength(1);
  for (const r of traffic) {
    expect(await r.headerValue("authorization")).toBeNull();
    if (r.method() !== "GET") expect(await r.headerValue("x-scribe-request")).toBe("1");
  }
  expect(starts[0]!.postData()).toBeNull(); // no body
  expect(logs.filter((l) => /pageerror|unhandled/i.test(l))).toEqual([]);
});

test("the downloaded backup restores through the browser into new libraries", async ({ page, context, product }, testInfo) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await seedTwo(product, kindle);
  const before = await libraries(product, kindle);
  const snapshotsBefore = await Promise.all(before.map(async (l) => (await product.call("GET", `/v1/libraries/${l.library_id}/snapshot`, { bearer: kindle })).json));
  await asAdmin(context, product, admin);
  await openSettings(page, product);

  // Backup, downloaded by the browser.
  await page.getByTestId("backup-create").click();
  const row = page.getByTestId("backup-export");
  await expect(row).toHaveAttribute("data-state", "ready", { timeout: 20_000 });
  const path = testInfo.outputPath("ignite-backup.zip");
  await downloadInBrowser(page, path);

  // The same saved file, uploaded through Restore, reviewed and confirmed.
  const card = page.getByTestId("restore-card");
  await card.getByTestId("restore-file").setInputFiles(path);
  const [up] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === "POST" && new URL(r.url()).pathname === RESTORE_JOBS),
    card.getByTestId("restore-upload-button").click(),
  ]);
  expect(up.status()).toBe(201);
  // The plan as the server reports it (the page's own copy of the 201 holds the one-time
  // confirmation and is not read here).
  const jobs = ((await product.call("GET", RESTORE_JOBS, { cookie: admin })).json as {
    jobs: { job_id: string; archive: { sha256: string }; libraries: { source_library_id: string; destination_library_id: string }[] }[];
  }).jobs;
  expect(jobs).toHaveLength(1);
  const prepared = jobs[0]!;
  expect(prepared.archive.sha256).toBe(sha256(path));
  const mapping = new Map(prepared.libraries.map((l) => [l.source_library_id, l.destination_library_id]));
  expect([...mapping.keys()].sort()).toEqual(before.map((l) => l.library_id).sort());
  const job = card.getByTestId("restore-job");
  await job.getByTestId("restore-review").click();
  await expect(job.getByTestId("restore-confirm-panel")).toContainText("Ignite will create 2 new libraries");
  await job.getByTestId("restore-ack").check();
  await job.getByTestId("restore-confirm-yes").click();
  await expect
    .poll(
      async () => {
        const s = await job.getAttribute("data-state");
        if (s !== "succeeded" && s !== "failed") await card.getByTestId("restore-refresh").click();
        return s;
      },
      { timeout: 20_000 },
    )
    .toBe("succeeded");
  await shot(page, "backup-restore-roundtrip-desktop");

  // New libraries hold the backed-up state; the originals are untouched; the blob is there.
  const after = await libraries(product, kindle);
  expect(after).toHaveLength(4);
  for (const b of before) expect(after.find((l) => l.library_id === b.library_id)).toEqual(b);
  for (const [i, b] of before.entries()) {
    expect((await product.call("GET", `/v1/libraries/${b.library_id}/snapshot`, { bearer: kindle })).json).toEqual(snapshotsBefore[i]);
  }
  for (const [src, dst] of mapping) {
    const source = (snapshotsBefore[before.findIndex((l) => l.library_id === src)] as { snapshot: Record<string, unknown> }).snapshot;
    const genesis = (await product.call("GET", `/v1/libraries/${dst}/genesis`, { bearer: kindle })).json as { genesis_sha256: string; snapshot: Record<string, unknown> };
    expect(genesis.snapshot).toEqual({ ...source, library_id: dst });
    const current = (await product.call("GET", `/v1/libraries/${dst}/snapshot`, { bearer: kindle })).json as { snapshot_sha256: string };
    expect(current.snapshot_sha256).toBe(genesis.genesis_sha256);
  }
  expect((await product.call("GET", `/v1/blobs/${manifest.blob.sha256}`, { bearer: kindle })).status).toBe(200);
  // The backup half never touched the restore: its export is still there until discarded.
  expect((await ownExports(product, admin)).map((e) => e.state)).toEqual(["ready"]);
});

test("a reload during preparation resumes following the export; a ready export is shown after a reload", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await seedLarge(product, kindle);
  await asAdmin(context, product, admin);
  const traffic = exportTraffic(page);
  await openSettings(page, product);
  const [start] = await Promise.all([page.waitForResponse((r) => isStart(r.request())), page.getByTestId("backup-create").click()]);
  expect(start.status()).toBe(202);
  await page.reload();
  const row = page.getByTestId("backup-export");
  await expect(row).toHaveAttribute("data-state", "preparing");
  expect((await ownExports(product, admin))[0]!.state).toBe("preparing"); // genuinely preparing on the server
  await expect(page.getByTestId("backup-create")).toBeDisabled();
  await expect(row).toHaveAttribute("data-state", "ready", { timeout: 45_000 });
  expect(traffic.filter(isStart)).toHaveLength(1); // the reload never re-requested it
  const lists = traffic.filter(isList).length;
  expect(lists).toBeGreaterThan(1); // followed by polling the list

  // Ready, then reloaded: shown again from the server, downloadable, nothing re-created.
  await page.reload();
  await expect(row).toHaveAttribute("data-state", "ready");
  await expect(row.getByTestId("backup-download")).toBeVisible();
  await expect(row.getByTestId("backup-libraries")).toHaveText("2");
  await page.waitForTimeout(2500); // longer than the poll interval: a ready export is not polled
  const settled = traffic.filter(isList).length;
  await page.waitForTimeout(2500);
  expect(traffic.filter(isList).length).toBe(settled);
  expect(traffic.filter(isStart)).toHaveLength(1);
});

test("a failed export offers no download; it can be discarded and a new one requested", async ({ page, context, product }) => {
  // No libraries at all: the server's export fails (`no_libraries`), for real.
  const admin = await product.claimAsHarness();
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  await page.getByTestId("backup-create").click();
  const row = page.getByTestId("backup-export");
  await expect(row).toHaveAttribute("data-state", "failed", { timeout: 20_000 });
  await expect(row.getByTestId("backup-state")).toHaveText("Backup failed");
  await expect(row.getByTestId("backup-failed")).toContainText("no_libraries");
  await expect(row.getByTestId("backup-download")).toHaveCount(0);
  await shot(page, "backup-failed-desktop");
  await expect(page.getByTestId("backup-create")).toBeEnabled(); // a failure does not block a new attempt
  await row.getByTestId("backup-discard").click();
  await row.getByTestId("backup-discard-yes").click();
  await expect(page.getByTestId("backup-export")).toHaveCount(0);
  expect(await ownExports(product, admin)).toEqual([]);
});

test("an expired export cannot be downloaded and must be created again", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  await page.getByTestId("backup-create").click();
  const row = page.getByTestId("backup-export");
  await expect(row).toHaveAttribute("data-state", "ready", { timeout: 20_000 });
  // The server's retention is 15 minutes; its answer is rewritten as it would read once expired.
  await page.route(`**${EXPORTS}`, async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    const real = await route.fetch();
    const body = (await real.json()) as { v: number; exports: Summary[] };
    for (const e of body.exports) Object.assign(e, { state: "expired", expires_in_s: 0, download: null });
    await route.fulfill({ response: real, json: body });
  });
  await page.getByTestId("backup-refresh").click();
  await expect(row).toHaveAttribute("data-state", "expired");
  await expect(row.getByTestId("backup-expired")).toContainText("Create a new backup");
  await expect(row.getByTestId("backup-download")).toHaveCount(0);
  await expect(page.getByTestId("backup-create")).toBeDisabled(); // the server still holds it
  await shot(page, "backup-expired-desktop");
  // Discarding it works against the real server.
  await page.unroute(`**${EXPORTS}`);
  await row.getByTestId("backup-discard").click();
  await row.getByTestId("backup-discard-yes").click();
  await expect(page.getByTestId("backup-export")).toHaveCount(0);
  await expect(page.getByTestId("backup-create")).toBeEnabled();
});

test("a download refused by the server (410/404 JSON) never replaces the settings page", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  await page.getByTestId("backup-create").click();
  const row = page.getByTestId("backup-export");
  await expect(row).toHaveAttribute("data-state", "ready", { timeout: 20_000 });
  const id = (await row.getAttribute("data-export-id"))!;
  // Withdrawn behind the page's back (another tab): the page still shows the stale link.
  expect((await product.call("DELETE", `${EXPORTS}/${id}`, { cookie: admin })).status).toBe(200);
  const url = page.url();
  page.on("download", () => undefined); // a browser may record the refused response as a failed download
  await row.getByTestId("backup-download").click();
  // The page stays, and the list refresh shows the export is gone.
  await expect(page.getByTestId("backup-export")).toHaveCount(0);
  expect(page.url()).toBe(url);
  await expect(page.getByTestId("server-settings")).toBeVisible();
  await expect(page.getByTestId("backup-create")).toBeEnabled();
  expect(await page.content()).not.toContain('"error":"not_found"');
});

test("a double click sends one request; the one-backup rule is explained", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  const traffic = exportTraffic(page);
  await openSettings(page, product);
  await page.getByTestId("backup-create").dblclick();
  await expect(page.getByTestId("backup-export")).toHaveAttribute("data-state", "ready", { timeout: 20_000 });
  expect(traffic.filter(isStart)).toHaveLength(1);
  await expect(page.getByTestId("backup-create")).toBeDisabled();
  await expect(page.getByTestId("backup-one-at-a-time")).toBeVisible();
  // The server is authoritative: a second request is refused and explained.
  const second = await product.call("POST", EXPORTS, { cookie: admin });
  expect([second.status, (second.json as { error: string }).error]).toEqual([429, "export_exists"]);
});

test("a lost start response is reported as unknown, never resent, and the export is found", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  const traffic = exportTraffic(page);
  let accepted = 0;
  await page.route(`**${EXPORTS}`, async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    const real = await route.fetch(); // the server accepts it...
    accepted = real.status();
    await route.abort("connectionreset"); // ...but the browser never hears back
  });
  await openSettings(page, product);
  await page.getByTestId("backup-create").click();
  const notice = page.getByTestId("backup-create-notice");
  await expect(notice).toContainText("Backup request outcome unknown.");
  await expect(notice).toHaveAttribute("data-kind", "uncertain_found");
  expect(accepted).toBe(202);
  const row = page.getByTestId("backup-export");
  await expect(row).toHaveAttribute("data-state", "ready", { timeout: 20_000 });
  await shot(page, "backup-uncertain-desktop");
  expect(traffic.filter(isStart)).toHaveLength(1);
  await expect(row.getByTestId("backup-download")).toBeVisible(); // recoverable
  expect(await ownExports(product, admin)).toHaveLength(1);
});

test("refused access drops every export from the page at once", async ({ page, context, product }) => {
  const owner = await product.claimAsHarness();
  const kindle = await product.pairKindle(owner);
  await product.seedFixture(kindle);
  const second = await product.secondAdmin();
  await asAdmin(context, product, second.cookie);
  await openSettings(page, product);
  await page.getByTestId("backup-create").click();
  const row = page.getByTestId("backup-export");
  await expect(row).toHaveAttribute("data-state", "ready", { timeout: 20_000 });
  const id = (await row.getAttribute("data-export-id"))!;
  // Another administrator revokes this browser. Whichever notices first (this section's next
  // request, or the app's own session check that signs the browser out), nothing privileged stays.
  expect((await product.call("POST", `/control/v1/admin/devices/${second.deviceId}/revoke`, { cookie: owner })).status).toBe(200);
  await page
    .getByTestId("backup-refresh")
    .click({ timeout: 2000 })
    .catch(() => undefined); // the app may already have left Settings
  await expect(page.getByTestId("backup-export")).toHaveCount(0);
  await expect(page.getByTestId("backup-sha")).toHaveCount(0);
  await expect(page.getByTestId("backup-download")).toHaveCount(0);
  // And the server refuses the revoked session's download.
  expect((await product.call("GET", `${EXPORTS}/${id}/download`, { cookie: second.cookie })).status).toBe(401);
});

test("an access refusal on the export list clears the section at once (the rest of Settings stays)", async ({ page, context, product }) => {
  const admin = await product.claimAsHarness();
  const kindle = await product.pairKindle(admin);
  await product.seedFixture(kindle);
  await asAdmin(context, product, admin);
  await openSettings(page, product);
  await page.getByTestId("backup-create").click();
  const row = page.getByTestId("backup-export");
  await expect(row).toHaveAttribute("data-state", "ready", { timeout: 20_000 });
  // As the server answers a browser that is not on its loopback address.
  await page.route(`**${EXPORTS}`, (r) =>
    r.fulfill({ status: 403, contentType: "application/json", body: JSON.stringify({ error: "loopback_required", message: "server text" }) }),
  );
  await page.getByTestId("backup-refresh").click();
  await expect(page.getByTestId("backup-error")).toHaveAttribute("data-kind", "loopback");
  await expect(page.getByTestId("backup-error")).toContainText("secure tunnel");
  await expect(page.getByTestId("backup-export")).toHaveCount(0);
  await expect(page.getByTestId("backup-sha")).toHaveCount(0);
  await expect(page.getByTestId("backup-create")).toHaveCount(0);
  await expect(page.getByTestId("server-card")).toBeVisible();
  await expect(page.getByTestId("restore-card")).toBeVisible();
});
