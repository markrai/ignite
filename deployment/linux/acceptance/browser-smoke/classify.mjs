/**
 * Distinguish critical browser resource failures from expected API 4xx.
 *
 * Precedence: critical document/script/stylesheet/WASM failures always win,
 * even when the URL path looks like `/v1/` or `/control/v1/`.
 */

/**
 * @param {string} url
 * @param {string} [resourceType] Playwright request.resourceType()
 * @returns {boolean}
 */
export function isCriticalResource(url, resourceType) {
  const u = String(url || "");
  const t = String(resourceType || "").toLowerCase();
  if (t === "document" || t === "script" || t === "stylesheet" || t === "wasm") return true;
  if (/\.wasm(\?|$)/i.test(u) || /scribe_web_wasm/i.test(u)) return true;
  if (/\/assets\//i.test(u) && /\.(js|css|mjs|wasm)(\?|$)/i.test(u)) return true;
  return false;
}

/**
 * Expected/benign control-plane or sync API statuses during smoke (not PASS-killers).
 * Only applies to non-critical resource types (fetch/xhr/other).
 * @param {string} url
 * @param {number} status
 */
export function isExpectedApiStatus(url, status) {
  const u = String(url || "");
  const s = Number(status);
  if (!(s >= 400 && s < 600)) return false;
  // Control / sync APIs: 4xx can be expected (e.g. 409 claimed); record but do not fail smoke.
  if (/\/control\/v1\//i.test(u) || /\/v1\//i.test(u)) {
    return s < 500;
  }
  return false;
}

/**
 * @param {{ url: string, status?: number, error?: string, resourceType?: string }} ev
 * @returns {"critical" | "expected_api" | "other"}
 */
export function classifyFailure(ev) {
  const url = ev.url || "";
  const status = ev.status;
  // Critical browser resources take precedence over API-path heuristics.
  if (isCriticalResource(url, ev.resourceType)) return "critical";
  if (ev.error && (/\.wasm(\?|$)/i.test(url) || /\/assets\/.+\.(js|css|mjs)/i.test(url))) {
    return "critical";
  }
  if (status != null && isExpectedApiStatus(url, status)) return "expected_api";
  return "other";
}
