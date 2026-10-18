#!/usr/bin/env node
/**
 * Negative-path regressions for smoke helpers (no browser launch).
 */
import { classifyFailure, isCriticalResource, isExpectedApiStatus } from "./classify.mjs";
import { remainingMs, stepTimeout } from "./deadline.mjs";
import { sanitizeText } from "./sanitize.mjs";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

function assert(cond, msg) {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

// --- deadline: must throw when exhausted (no 5s floor) ---
let threw = false;
try {
  remainingMs(Date.now() - 1);
} catch (e) {
  threw = e instanceof Error && /deadline exceeded/i.test(e.message);
}
assert(threw, "remainingMs throws after deadline");

const dl = Date.now() + 8_000;
const step = stepTimeout(dl, 45_000);
assert(step <= 8_000 && step > 0, `stepTimeout capped by remaining, got ${step}`);

threw = false;
try {
  stepTimeout(Date.now() - 50, 45_000);
} catch {
  threw = true;
}
assert(threw, "stepTimeout throws when deadline already passed");

// --- classify: critical vs expected API ---
assert(isCriticalResource("http://127.0.0.1:8787/assets/x.js", "script"), "script critical");
assert(isCriticalResource("http://127.0.0.1:8787/assets/a.wasm", "wasm"), "wasm critical");
assert(isCriticalResource("http://127.0.0.1:8787/", "document"), "document critical");
assert(isExpectedApiStatus("http://127.0.0.1:8787/control/v1/claim", 409), "409 claim expected");
assert(isExpectedApiStatus("http://127.0.0.1:8787/v1/libraries", 404), "404 sync api expected");
assert(!isExpectedApiStatus("http://127.0.0.1:8787/assets/app.js", 404), "404 script not expected-api");

assert(
  classifyFailure({ url: "http://x/assets/app.js", status: 404, resourceType: "script" }) === "critical",
  "script 404 is critical",
);
assert(
  classifyFailure({ url: "http://x/control/v1/claim", status: 409, resourceType: "fetch" }) === "expected_api",
  "claim 409 is expected_api",
);
assert(
  classifyFailure({ url: "http://x/assets/scribe_web_wasm.wasm", error: "net::ERR", resourceType: "wasm" }) ===
    "critical",
  "wasm requestfailed is critical",
);

// Precedence: critical resource type wins even when URL looks like an API path.
assert(
  classifyFailure({ url: "http://x/v1/bundle/app.js", status: 404, resourceType: "script" }) === "critical",
  "failed script under /v1/ is critical (not expected_api)",
);
assert(
  classifyFailure({ url: "http://x/control/v1/theme.css", status: 500, resourceType: "stylesheet" }) === "critical",
  "failed stylesheet under /control/v1/ is critical",
);
assert(
  classifyFailure({ url: "http://x/v1/libraries/missing", status: 404, resourceType: "fetch" }) === "expected_api",
  "ordinary API 404 remains expected_api",
);

// --- smoke.mjs must pass options as 3rd arg to waitForFunction ---
const smokeSrc = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "smoke.mjs"), "utf8");
assert(
  /waitForFunction\s*\(\s*\(\)\s*=>[\s\S]*?,\s*undefined\s*,\s*\{\s*timeout:/.test(smokeSrc),
  "waitForFunction must use (fn, undefined, { timeout }) — not options as 2nd arg",
);
assert(!/remaining\s*=\s*\(\)\s*=>\s*Math\.max\s*\(\s*5_?000/.test(smokeSrc), "must not floor remaining at 5000ms");
assert(/setup-form.*count|getByTestId\("setup-form"\)\.count/.test(smokeSrc), "screenshot gated on setup-form absence");
assert(/console_error_count/.test(smokeSrc), "must not dump raw console text — count only");
assert(/critical_failures/.test(smokeSrc), "critical failures tracked for PASS gate");

// --- sanitize refuses leaking live setup code into evidence-shaped JSON ---
const secret = "zz11-yy22";
const fakeEvidence = JSON.stringify({
  failures: [`bad code ${secret}`],
  setup_code: "present_in_env_not_persisted",
});
const scrubbed = sanitizeText(fakeEvidence, [secret]);
assert(!scrubbed.includes(secret), "evidence JSON scrub removes live setup code");

console.log("PASS: browser-smoke helper negative-path regressions");
process.exit(0);
