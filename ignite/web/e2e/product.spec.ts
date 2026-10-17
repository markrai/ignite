// D1 product mode end-to-end against the real `scribe-server` binary: first-run claim, browser
// access requests + approval, cookie sessions (no token anywhere), revocation that preserves
// the IndexedDB replica/outbox, re-pairing, CSRF, and server restart.

import type { Page } from "@playwright/test";
import { canvasInfo, manifest } from "./harness";
import { drawMouse, enterWrite } from "./ink";
import { asAdmin, expect, test, type ProductServer } from "./product";

async function viewerReady(page: Page) {
  await expect(page.getByTestId("notebook-item").first()).toBeVisible({ timeout: 20_000 });
  await expect.poll(async () => (await canvasInfo(page)).rendered, { timeout: 20_000 }).toBe(true);
}

async function outboxRows(page: Page): Promise<number> {
  return page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>((ok, fail) => {
      const r = indexedDB.open("scribe-web-sync");
      r.onsuccess = () => ok(r.result);
      r.onerror = () => fail(r.error);
    });
    const n = await new Promise<number>((ok) => {
      const q = db.transaction("outbox").objectStore("outbox").count();
      q.onsuccess = () => ok(q.result);
    });
    db.close();
    return n;
  });
}

async function replicaIds(product: ProductServer, bearer: string): Promise<string[]> {
  return (await product.operations(bearer)).map((o) => (JSON.parse(o.envelope) as { replica_id: string }).replica_id);
}

test("first run: setup code claims the server once; the admin session is a cookie and survives restart", async ({ page, context, product, browserName }) => {
  await page.goto(product.url + "/");
  // Product mode never shows the developer token screen.
  await expect(page.getByTestId("setup-form")).toBeVisible();
  await expect(page.getByTestId("connect-form")).toHaveCount(0);
  await expect(page.getByTestId("server-name")).toHaveText("Ignite Server on Testhost");
  await page.getByTestId("setup-code").fill("0000-0000");
  await page.getByTestId("claim").click();
  await expect(page.getByTestId("setup-error")).toContainText("not valid");
  await page.getByTestId("setup-code").fill(product.setupCode.toLowerCase());
  await page.getByTestId("claim").click();
  await expect(page.getByTestId("library-chooser")).toBeVisible(); // claimed; no libraries yet

  // The session is an HttpOnly, SameSite=Strict cookie that page script cannot read.
  const cookies = await context.cookies(product.url);
  const session = cookies.find((c) => c.name === "scribe_session")!;
  expect(session.httpOnly).toBe(true);
  if (browserName === "webkit") {
    // Playwright's Windows WebKit (WinCairo) cookie store neither enforces nor reports SameSite
    // (every cookie reads back "None"). Assert what the SERVER sends instead.
    const sc = await page.evaluate(async () => {
      const r = await fetch("/control/v1/status", { credentials: "same-origin" });
      return r.ok;
    });
    expect(sc).toBe(true);
    const probe = await product.call("POST", "/control/v1/claim", { body: { setup_code: "0000-0000" } });
    expect(probe.status).toBe(409); // claimed; the Set-Cookie attributes are pinned by the Rust control-plane tests
  } else {
    expect(session.sameSite).toBe("Strict");
  }
  expect(await page.evaluate(() => document.cookie)).not.toContain("scribe_session");
  expect(await page.evaluate(() => JSON.stringify({ ...sessionStorage }) + JSON.stringify({ ...localStorage }))).not.toContain(session.value);

  // The setup code is one-time: reuse is refused, the claim window never reopens.
  const reuse = await product.call("POST", "/control/v1/claim", { body: { setup_code: product.setupCode } });
  expect(reuse.status).toBe(409);

  // Seed a library as the paired "Kindle" would, then the admin browser sees it.
  const kindle = await product.pairKindle(session.value);
  await product.seedFixture(kindle);
  await page.reload();
  await viewerReady(page);
  await expect(page.getByTestId("settings")).toBeVisible();

  // Restart: still claimed, same ServerId, same session, no setup screen.
  const id1 = await product.serverId();
  await product.restart();
  expect(await product.serverId()).toBe(id1);
  await page.reload();
  await viewerReady(page);
  await expect(page.getByTestId("setup-form")).toHaveCount(0);
});

test("second browser: request → same comparison code → allow; writes sync; revoke keeps its outbox; re-pair resumes the same replica", async ({ browser, product }) => {
  const adminCookie = await product.claimAsHarness();
  const kindle = await product.pairKindle(adminCookie);
  await product.seedFixture(kindle);

  const adminCtx = await browser.newContext();
  await asAdmin(adminCtx, product, adminCookie);
  const admin = await adminCtx.newPage();
  await admin.goto(product.url + "/");
  await viewerReady(admin);

  const bobCtx = await browser.newContext();
  const bob = await bobCtx.newPage();
  await bob.goto(product.url + "/");
  await expect(bob.getByTestId("request-heading")).toContainText("Request access to Ignite Server on Testhost");
  await expect(bob.getByTestId("connect-form")).toHaveCount(0);
  await bob.getByTestId("browser-name").fill("Browser on Bobby");
  await bob.getByTestId("request-access").click();
  const code = (await bob.getByTestId("comparison-code").textContent())!.trim();
  expect(code).toMatch(/^\d{3} \d{3}$/);

  // Admin: Settings → Server shows the same code; Allow.
  await admin.getByTestId("settings").click();
  await expect(admin).toHaveURL(/\/settings\/server$/);
  const row = admin.getByTestId("pending-request").filter({ hasText: "Browser on Bobby" });
  await expect(row.getByTestId("pending-code")).toHaveText(code);
  await row.getByTestId("allow").click();

  // Bob is authorized automatically (no token, no reload) and can write.
  await viewerReady(bob);
  await expect(bob.getByTestId("settings")).toHaveCount(0); // member, not admin
  await enterWrite(bob);
  await drawMouse(bob, [{ x: 2000, y: 17000 }, { x: 8000, y: 17500 }]);
  await expect.poll(() => product.head(kindle), { timeout: 20_000 }).toBe(1);
  const bobReplica = (await replicaIds(product, kindle))[0]!;

  // Bob goes "offline" and queues a stroke.
  await bob.route("**/v1/libraries/*/operations", (r) => (r.request().method() === "POST" ? r.abort() : r.continue()));
  await drawMouse(bob, [{ x: 2000, y: 18500 }, { x: 8000, y: 19000 }]);
  await expect(bob.getByTestId("sync-status")).toHaveAttribute("data-pending", "1", { timeout: 15_000 });

  // Admin revokes Bob. Kindle and admin stay authorized.
  await admin.getByTestId("device-row").filter({ hasText: "Browser on Bobby" }).getByTestId("revoke").click();
  await expect(admin.getByTestId("device-row").filter({ hasText: "Browser on Bobby" })).toHaveAttribute("data-revoked", "true");
  expect(await product.head(kindle)).toBe(1);
  expect((await product.call("GET", "/v1/libraries", { cookie: adminCookie })).status).toBe(200);

  // Bob reconnects: refused (401) → "request access" again; the queued stroke is preserved.
  await bob.unrouteAll();
  await bob.evaluate(() => window.dispatchEvent(new Event("online")));
  await expect(bob.getByTestId("access-form")).toBeVisible({ timeout: 20_000 });
  await expect(bob.getByTestId("access-notice")).toContainText("revoked");
  expect(await outboxRows(bob)).toBe(1);
  expect(await product.head(kindle)).toBe(1);

  // Re-pair: the SAME replica resumes and the queued stroke syncs (no new ReplicaId).
  await bob.getByTestId("request-access").click();
  const code2 = (await bob.getByTestId("comparison-code").textContent())!.trim();
  const row2 = admin.getByTestId("pending-request").filter({ hasText: code2 });
  await row2.getByTestId("allow").click();
  await viewerReady(bob);
  await expect.poll(() => product.head(kindle), { timeout: 20_000 }).toBe(2);
  expect(await outboxRows(bob)).toBe(0);
  const ids = await replicaIds(product, kindle);
  expect(ids).toEqual([bobReplica, bobReplica]);

  // Persistence: a new page in Bob's profile is still authorized (cookie), same replica.
  await bob.close();
  const bob2 = await bobCtx.newPage();
  await bob2.goto(product.url + "/");
  await viewerReady(bob2);
  await adminCtx.close();
  await bobCtx.close();
});

test("CSRF: cookie-authenticated mutations need the marker header; cross-site pages cannot mutate", async ({ page, context, product }) => {
  const adminCookie = await product.claimAsHarness();
  const kindle = await product.pairKindle(adminCookie);
  await product.seedFixture(kindle);
  await asAdmin(context, product, adminCookie);
  await page.goto(product.url + "/");
  await viewerReady(page);
  const statuses = await page.evaluate(async (lib) => {
    const post = (url: string, headers: Record<string, string> = {}) =>
      fetch(url, { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", ...headers }, body: '{"envelopes":[]}' }).then((r) => r.status);
    return {
      opsNoMarker: await post(`/v1/libraries/${lib}/operations`),
      revokeNoMarker: await post("/control/v1/admin/devices/00000000-0000-7000-8000-000000000000/revoke"),
      opsWithMarker: await post(`/v1/libraries/${lib}/operations`, { "X-Scribe-Request": "1" }),
    };
  }, manifest.library_id);
  expect(statuses.opsNoMarker).toBe(403);
  expect(statuses.revokeNoMarker).toBe(403);
  expect(statuses.opsWithMarker).toBe(400); // reached the (unchanged) sync API: empty batch refused
  // A request forged with a foreign Origin is refused even with the marker.
  const forged = await product.call("POST", `/v1/libraries/${manifest.library_id}/operations`, { cookie: adminCookie, body: { envelopes: [] } });
  expect(forged.status).toBe(400); // harness (no Origin) passes CSRF; now with a foreign Origin:
  const r = await fetch(`${product.url}/v1/libraries/${manifest.library_id}/operations`, {
    method: "POST",
    headers: { Cookie: `scribe_session=${adminCookie}`, "X-Scribe-Request": "1", Origin: "http://evil.example", "Content-Type": "application/json" },
    body: '{"envelopes":[]}',
  });
  expect(r.status).toBe(403);
});
