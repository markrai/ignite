/**
 * Distinguish critical browser resource failures from expected API 4xx.
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
  // Main document navigations
  if (t === "document") return true;
  return false;
}

/**
 * Expected/benign control-plane or sync API statuses during smoke (not PASS-killers).
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
  if (status != null && isExpectedApiStatus(url, status)) return "expected_api";
  if (isCriticalResource(url, ev.resourceType)) return "critical";
  if (ev.error && isCriticalResource(url, ev.resourceType || "document")) return "critical";
  // requestfailed on script/document/wasm without status
  if (ev.error && isCriticalResource(url, ev.resourceType)) return "critical";
  if (ev.error && (/\.wasm(\?|$)/i.test(url) || /\/assets\/.+\.js/i.test(url))) return "critical";
  return "other";
}
