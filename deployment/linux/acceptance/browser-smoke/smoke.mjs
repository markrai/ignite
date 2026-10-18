#!/usr/bin/env node
/**
 * Playwright Chromium smoke against a live Ignite Server (systemd / Multipass).
 *
 * Required env:
 *   IGNITE_ACCEPT_SETUP_CODE  — one-time setup code (memory only; never written)
 *
 * Optional env:
 *   IGNITE_ACCEPT_BASE_URL    — default http://127.0.0.1:8787
 *   IGNITE_ACCEPT_EVIDENCE    — directory for structured evidence
 *   IGNITE_ACCEPT_FIXTURES    — fixture dir (manifest + snapshot + blob); default ./fixtures
 *   IGNITE_ACCEPT_SMOKE_TIMEOUT_MS — overall budget (default 180000)
 *   IGNITE_ACCEPT_SESSION_FILE — if set, write `scribe_session=…` (chmod 0600) for callers;
 *                                must be outside evidence (e.g. accept scratch). Never under evidence.
 *
 * Exit 0 only when every required assertion passes → browser_class=playwright-chromium.
 * Always writes browser-smoke.json (sanitized), including on failure.
 */

import { chromium } from "playwright";
import { readFileSync, writeFileSync, mkdirSync, existsSync, chmodSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { classifyFailure } from "./classify.mjs";
import { remainingMs, stepTimeout } from "./deadline.mjs";
import { sanitizeLogLines, sanitizeText, sanitizeUrl } from "./sanitize.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const BASE = (process.env.IGNITE_ACCEPT_BASE_URL || "http://127.0.0.1:8787").replace(/\/$/, "");
const EVIDENCE = process.env.IGNITE_ACCEPT_EVIDENCE || join(HERE, "evidence-out");
const FIXTURES = resolve(process.env.IGNITE_ACCEPT_FIXTURES || join(HERE, "fixtures"));
const SETUP_CODE = process.env.IGNITE_ACCEPT_SETUP_CODE || "";
const SESSION_FILE = process.env.IGNITE_ACCEPT_SESSION_FILE || "";
const SECRETS = SETUP_CODE ? [SETUP_CODE] : [];
const BUDGET_MS = Number(process.env.IGNITE_ACCEPT_SMOKE_TIMEOUT_MS || 180_000);
const NAV_TIMEOUT = 60_000;
const STEP_TIMEOUT = 45_000;

/** @type {{
 *  ok: boolean,
 *  browser_class: string | null,
 *  assertions: Record<string, boolean | string>,
 *  failures: string[],
 *  page_errors: string[],
 *  console_error_count: number,
 *  critical_failures: { url: string, status?: number, error?: string, kind: string }[],
 *  expected_api_4xx_count: number,
 *  wasm: { http_ok: boolean, initialized: boolean, app_op: boolean },
 *  auth: { cookie_httponly: boolean, api_ok: boolean, reload_ok: boolean },
 *  screenshot: string | null,
 * }} */
const report = {
  ok: false,
  browser_class: null,
  assertions: {},
  failures: [],
  page_errors: [],
  console_error_count: 0,
  critical_failures: [],
  expected_api_4xx_count: 0,
  wasm: { http_ok: false, initialized: false, app_op: false },
  auth: { cookie_httponly: false, api_ok: false, reload_ok: false },
  screenshot: null,
};

function scrub(msg) {
  return sanitizeText(msg, SECRETS);
}

function fail(msg) {
  report.failures.push(scrub(msg));
  console.error(`SMOKE FAIL: ${scrub(msg)}`);
}

function pass(key, detail) {
  report.assertions[key] = detail === undefined ? true : scrub(String(detail));
  console.log(`SMOKE OK: ${key}${detail !== undefined ? ` (${scrub(String(detail))})` : ""}`);
}

function writeEvidence() {
  mkdirSync(EVIDENCE, { recursive: true });
  const out = {
    ok: report.ok,
    browser_class: report.browser_class,
    assertions: report.assertions,
    failures: report.failures.map((f) => scrub(f)),
    // Actionable only: uncaught exceptions + critical asset failures (not raw console dumps).
    page_errors: sanitizeLogLines(report.page_errors, SECRETS, 20),
    console_error_count: report.console_error_count,
    critical_failures: report.critical_failures.slice(0, 40).map((r) => ({
      url: sanitizeUrl(r.url),
      status: r.status,
      error: r.error ? scrub(r.error).slice(0, 200) : undefined,
      kind: r.kind,
    })),
    expected_api_4xx_count: report.expected_api_4xx_count,
    wasm: report.wasm,
    auth: report.auth,
    screenshot: report.screenshot,
    setup_code: "present_in_env_not_persisted",
    base_url: sanitizeUrl(BASE + "/"),
  };
  // Final pass: scrub any leaked secret from the serialized JSON.
  const json = scrub(JSON.stringify(out, null, 2) + "\n");
  writeFileSync(join(EVIDENCE, "browser-smoke.json"), json, "utf8");
}

function loadFixtures() {
  const manifestPath = join(FIXTURES, "fixture-manifest.json");
  if (!existsSync(manifestPath)) {
    throw new Error(`fixtures missing: ${manifestPath}`);
  }
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  const snapshotPath = join(FIXTURES, manifest.snapshot || "snapshot-v1.json");
  const blobRel = manifest.blob?.file;
  if (!blobRel) throw new Error("fixture manifest missing blob.file");
  const blobPath = join(FIXTURES, blobRel);
  return {
    manifest,
    snapshotText: readFileSync(snapshotPath, "utf8"),
    blob: readFileSync(blobPath),
    blobSha: manifest.blob.sha256,
    libraryId: manifest.library_id,
  };
}

/**
 * @param {import('playwright').APIRequestContext} request
 * @param {string} cookieHeader
 */
async function pairAndSeed(request, cookieHeader, fix) {
  const hdrs = {
    Cookie: cookieHeader,
    "X-Scribe-Request": "1",
    "Content-Type": "application/json",
  };
  const pairStart = await request.post(`${BASE}/control/v1/pairing`, {
    headers: hdrs,
    data: { client_type: "kindle", display_name: "AcceptSmoke" },
  });
  if (!pairStart.ok()) {
    throw new Error(`pairing start HTTP ${pairStart.status()}`);
  }
  const pairBody = await pairStart.json();
  const requestId = pairBody.request_id;
  const requestSecret = pairBody.request_secret;
  if (!requestId || !requestSecret) throw new Error("pairing start missing ids");

  const approve = await request.post(`${BASE}/control/v1/admin/pairing/${requestId}/approve`, {
    headers: hdrs,
  });
  if (!approve.ok()) throw new Error(`pairing approve HTTP ${approve.status()}`);

  const poll = await request.post(`${BASE}/control/v1/pairing/${requestId}/poll`, {
    headers: { "X-Scribe-Request": "1", "Content-Type": "application/json" },
    data: { request_secret: requestSecret },
  });
  if (!poll.ok()) throw new Error(`pairing poll HTTP ${poll.status()}`);
  const cred = (await poll.json()).credential;
  if (!cred) throw new Error("pairing poll missing credential");

  const putBlob = await request.put(`${BASE}/v1/blobs/${fix.blobSha}`, {
    headers: { Authorization: `Bearer ${cred}` },
    data: fix.blob,
  });
  if (putBlob.status() >= 300) throw new Error(`blob put HTTP ${putBlob.status()}`);

  const postLib = await request.post(`${BASE}/v1/libraries`, {
    headers: { Authorization: `Bearer ${cred}`, "Content-Type": "application/json" },
    data: fix.snapshotText,
  });
  if (postLib.status() !== 201 && postLib.status() !== 200) {
    throw new Error(`libraries post HTTP ${postLib.status()}`);
  }
}

function recordNetworkFailure(ev) {
  const kind = classifyFailure(ev);
  if (kind === "expected_api") {
    report.expected_api_4xx_count += 1;
    return;
  }
  if (kind === "critical") {
    report.critical_failures.push({
      url: ev.url,
      status: ev.status,
      error: ev.error,
      kind: ev.resourceType || "critical",
    });
  }
  // "other" network noise is counted only via console_error_count if surfaced there.
}

async function maybeScreenshot(page, deadline) {
  remainingMs(deadline);
  const setupCount = await page.getByTestId("setup-form").count();
  if (setupCount !== 0) {
    fail("refusing screenshot: setup-form still present (would risk capturing setup code)");
    return;
  }
  const shotPath = join(EVIDENCE, "browser-after-claim.png");
  await page.screenshot({ path: shotPath, fullPage: false, timeout: stepTimeout(deadline, 15_000) });
  report.screenshot = "browser-after-claim.png";
  pass("screenshot_after_claim");
}

async function main() {
  const deadline = Date.now() + BUDGET_MS;
  const to = (cap) => stepTimeout(deadline, cap);

  if (!SETUP_CODE || SETUP_CODE.length < 8) {
    fail("IGNITE_ACCEPT_SETUP_CODE missing or too short");
    writeEvidence();
    process.exit(1);
  }

  let fix;
  try {
    fix = loadFixtures();
  } catch (e) {
    fail(`fixture load: ${e instanceof Error ? e.message : e}`);
    writeEvidence();
    process.exit(1);
  }

  mkdirSync(EVIDENCE, { recursive: true });
  console.log(`SMOKE: base=${sanitizeUrl(BASE + "/")} evidence=${EVIDENCE} budget_ms=${BUDGET_MS}`);

  const browser = await chromium.launch({
    headless: true,
    timeout: to(60_000),
  });
  const context = await browser.newContext();
  const page = await context.newPage();
  page.setDefaultTimeout(to(STEP_TIMEOUT));

  /** @type {boolean} */
  let wasmHttpOk = false;

  page.on("console", (msg) => {
    if (msg.type() === "error") {
      report.console_error_count += 1;
      // Do not persist arbitrary console text — count only.
    }
  });
  page.on("pageerror", (err) => {
    report.page_errors.push(scrub(err.message || String(err)));
  });
  page.on("response", (resp) => {
    const u = resp.url();
    const req = resp.request();
    const resourceType = req.resourceType();
    if (/\.wasm(\?|$)/i.test(u) || /scribe_web_wasm/i.test(u)) {
      if (resp.ok()) wasmHttpOk = true;
      else recordNetworkFailure({ url: u, status: resp.status(), resourceType: "wasm" });
      return;
    }
    if (resp.status() >= 400) {
      recordNetworkFailure({ url: u, status: resp.status(), resourceType });
    }
  });
  page.on("requestfailed", (req) => {
    recordNetworkFailure({
      url: req.url(),
      error: req.failure()?.errorText || "requestfailed",
      resourceType: req.resourceType(),
    });
  });

  try {
    remainingMs(deadline);
    await page.goto(`${BASE}/`, { waitUntil: "domcontentloaded", timeout: to(NAV_TIMEOUT) });
    await page.getByTestId("setup-form").waitFor({ state: "visible", timeout: to(STEP_TIMEOUT) });
    pass("setup_form_visible");

    await page.getByTestId("setup-code").fill(SETUP_CODE);
    await page.getByTestId("claim").click();
    await page.getByTestId("library-chooser").waitFor({ state: "visible", timeout: to(STEP_TIMEOUT) });
    pass("claim_ui_library_chooser");

    const cookies = await context.cookies(BASE);
    const session = cookies.find((c) => c.name === "scribe_session");
    if (!session?.value) {
      fail("scribe_session cookie missing after claim");
    } else if (!session.httpOnly) {
      fail("scribe_session cookie is not HttpOnly");
    } else {
      report.auth.cookie_httponly = true;
      pass("auth_cookie_httponly");
    }

    const cookieHeader = session?.value ? `scribe_session=${session.value}` : "";
    if (SESSION_FILE && cookieHeader) {
      // Caller-owned scratch path only — never write session material into EVIDENCE.
      writeFileSync(SESSION_FILE, cookieHeader + "\n", { encoding: "utf8", mode: 0o600 });
      try {
        chmodSync(SESSION_FILE, 0o600);
      } catch {
        /* best-effort on platforms without chmod */
      }
      pass("session_file_written");
    }
    const docCookie = await page.evaluate(() => document.cookie);
    if (session?.value && docCookie.includes(session.value)) {
      fail("session cookie value visible to document.cookie");
    } else {
      pass("auth_cookie_not_in_document");
    }

    const statusResp = await page.evaluate(async () => {
      const r = await fetch("/control/v1/status", { credentials: "same-origin", headers: { "X-Scribe-Request": "1" } });
      const json = await r.json();
      return { ok: r.ok, claimed: !!json.claimed, session: !!json.session, server_id: json.server_id || null };
    });
    if (!statusResp.ok || !statusResp.claimed || !statusResp.session) {
      fail(`authenticated status failed ok=${statusResp.ok} claimed=${statusResp.claimed} session=${statusResp.session}`);
    } else {
      report.auth.api_ok = true;
      pass("auth_api_status", statusResp.server_id ? "server_id_present" : "ok");
    }

    remainingMs(deadline);
    await page.reload({ waitUntil: "domcontentloaded", timeout: to(NAV_TIMEOUT) });
    await page.getByTestId("library-chooser").waitFor({ state: "visible", timeout: to(STEP_TIMEOUT) });
    const setupGone = (await page.getByTestId("setup-form").count()) === 0;
    if (!setupGone) {
      fail("setup-form still visible after reload (session did not survive)");
    } else {
      report.auth.reload_ok = true;
      pass("auth_reload_survives");
    }

    await maybeScreenshot(page, deadline);

    if (!cookieHeader) throw new Error("no session cookie for seeding");
    console.log("SMOKE: pairing Kindle + seeding fixture library…");
    remainingMs(deadline);
    await pairAndSeed(context.request, cookieHeader, fix);
    pass("fixture_seeded");

    remainingMs(deadline);
    await page.reload({ waitUntil: "domcontentloaded", timeout: to(NAV_TIMEOUT) });
    const card = page.getByTestId("library-card").first();
    const notebook = page.getByTestId("notebook-item").first();
    try {
      await notebook.waitFor({ state: "visible", timeout: to(15_000) });
      pass("library_auto_opened");
    } catch {
      remainingMs(deadline);
      await card.waitFor({ state: "visible", timeout: to(STEP_TIMEOUT) });
      await card.click();
      pass("library_card_opened");
    }

    await page.getByTestId("notebook-item").first().waitFor({ state: "visible", timeout: to(STEP_TIMEOUT) });
    pass("notebook_item_visible");

    // Playwright signature: waitForFunction(fn, arg, options) — options must be 3rd arg.
    await page.waitForFunction(
      () => {
        const el = document.querySelector('[data-testid="page-canvas"]');
        return el instanceof HTMLElement && el.dataset.rendered === "true";
      },
      undefined,
      { timeout: to(STEP_TIMEOUT) },
    );
    pass("canvas_data_rendered");

    report.wasm.http_ok = wasmHttpOk;
    if (!wasmHttpOk) {
      fail("no successful WASM HTTP response observed (asset may not have loaded)");
    } else {
      pass("wasm_http_ok");
    }

    const wasmInit = await page.evaluate(async () => {
      const idb = await new Promise((ok) => {
        try {
          const r = indexedDB.open("scribe-web-sync");
          r.onsuccess = () => {
            const db = r.result;
            const has = db.objectStoreNames.contains("replica") || db.objectStoreNames.contains("outbox");
            db.close();
            ok(has);
          };
          r.onerror = () => ok(false);
        } catch {
          ok(false);
        }
      });
      return { replicaIdb: idb };
    });
    if (!wasmInit.replicaIdb) {
      fail("WASM-dependent replica IndexedDB not opened after viewer load");
    } else {
      report.wasm.initialized = true;
      pass("wasm_initialized_replica_idb");
    }

    if (report.wasm.initialized && report.assertions.canvas_data_rendered) {
      report.wasm.app_op = true;
      pass("wasm_app_op_canvas_project");
    } else {
      fail("WASM-dependent application operation not confirmed");
    }

    if (report.page_errors.length > 0) {
      fail(`uncaught page errors: ${report.page_errors.length}`);
    } else {
      pass("no_page_errors");
    }

    if (report.critical_failures.length > 0) {
      fail(`critical asset/browser failures: ${report.critical_failures.length}`);
    } else {
      pass("no_critical_asset_failures");
    }

    const required = [
      report.assertions.setup_form_visible,
      report.assertions.claim_ui_library_chooser,
      report.auth.cookie_httponly,
      report.auth.api_ok,
      report.auth.reload_ok,
      report.wasm.http_ok,
      report.wasm.initialized,
      report.wasm.app_op,
      report.assertions.no_page_errors,
      report.assertions.no_critical_asset_failures,
      report.critical_failures.length === 0,
      report.page_errors.length === 0,
      report.failures.length === 0,
    ];
    if (required.every(Boolean)) {
      report.ok = true;
      report.browser_class = "playwright-chromium";
      console.log("SMOKE PASS browser_class=playwright-chromium");
    } else {
      fail("one or more required browser assertions failed");
    }
  } catch (e) {
    fail(e instanceof Error ? e.message : String(e));
  } finally {
    writeEvidence();
    await browser.close().catch(() => {});
  }

  process.exit(report.ok ? 0 : 1);
}

main().catch((e) => {
  fail(e instanceof Error ? e.message : String(e));
  writeEvidence();
  process.exit(1);
});
