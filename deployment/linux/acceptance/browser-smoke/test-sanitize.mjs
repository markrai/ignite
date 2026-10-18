#!/usr/bin/env node
/** Focused regression: sanitize.mjs must redact secrets and query strings. */
import { sanitizeText, sanitizeUrl, sanitizeLogLines } from "./sanitize.mjs";

function assert(cond, msg) {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const cookieLine = "Set-Cookie: scribe_session=abcDEF123.secret+value; Path=/; HttpOnly";
assert(sanitizeText(cookieLine).includes("[REDACTED]"), "cookie redacted");
assert(!sanitizeText(cookieLine).includes("abcDEF123"), "cookie value removed");

const setupJson = '{"setup_code":"ab12-cd34","server_id":"x"}';
assert(sanitizeText(setupJson).includes('"setup_code":"[REDACTED]"'), "setup_code json redacted");
assert(!sanitizeText(setupJson).includes("ab12-cd34"), "setup code value removed");

// Exact in-memory setup code must be scrubbed even outside JSON keys.
const live = "xy9k-m2pq";
const leak = `filled setup code ${live} into input`;
assert(!sanitizeText(leak, [live]).includes(live), "exact secret scrubbed");
assert(sanitizeText(leak, [live]).includes("[REDACTED]"), "exact secret replaced");

const bearer = "Authorization: Bearer eyJhbGciOi.secret";
assert(sanitizeText(bearer).includes("[REDACTED]"), "bearer redacted");

const url = "http://127.0.0.1:8787/v1/libraries?token=secret&x=1";
assert(sanitizeUrl(url) === "http://127.0.0.1:8787/v1/libraries", "url path only");
assert(sanitizeText(url).includes("?[REDACTED]"), "query stripped in text");

const lines = sanitizeLogLines([cookieLine, "ok", setupJson], [], 2);
assert(lines.length === 2, "log cap");
assert(lines[0].includes("[REDACTED]"), "log line sanitized");

console.log("PASS: browser-smoke sanitize regression");
process.exit(0);
