/**
 * Redact secrets from browser-smoke evidence text.
 * Never persist setup codes, session cookies, Bearer tokens, or query strings.
 */

const SETUP_CODE_LINE = /^(\s*)([A-Za-z0-9]{4}-[A-Za-z0-9]{4})(\s*)$/;

/**
 * Escape a string for use in a RegExp.
 * @param {string} s
 */
function escapeRegExp(s) {
  return String(s).replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * @param {string} input
 * @param {string[]} [secrets] exact secret values to scrub (e.g. in-memory setup code)
 * @returns {string}
 */
export function sanitizeText(input, secrets = []) {
  if (input == null) return "";
  let s = String(input);
  for (const secret of secrets) {
    if (!secret || String(secret).length < 4) continue;
    const re = new RegExp(escapeRegExp(String(secret)), "gi");
    s = s.replace(re, "[REDACTED]");
  }
  s = s.replace(/[Ss]et-[Cc]ookie:\s*scribe_session=[^;\s]+/gi, "Set-Cookie: scribe_session=[REDACTED]");
  s = s.replace(/scribe_session=[^;\s]+/gi, "scribe_session=[REDACTED]");
  s = s.replace(/"setup_code"\s*:\s*"[^"]+"/gi, '"setup_code":"[REDACTED]"');
  s = s.replace(/setup_code=[^&\s]+/gi, "setup_code=[REDACTED]");
  s = s.replace(/Bearer\s+[A-Za-z0-9._~+/=-]+/gi, "Bearer [REDACTED]");
  s = s.replace(/Authorization:\s*[^\r\n]+/gi, "Authorization: [REDACTED]");
  // Strip query strings from URLs (may carry tokens).
  s = s.replace(/\b(https?:\/\/[^\s"'<>?]+)\?[^\s"'<>]*/gi, "$1?[REDACTED]");
  s = s
    .split("\n")
    .map((line) => {
      if (SETUP_CODE_LINE.test(line) && !/REDACTED|setup_code|server_id/i.test(line)) {
        return line.replace(SETUP_CODE_LINE, "$1[REDACTED]$3");
      }
      return line;
    })
    .join("\n");
  return s;
}

/**
 * Path-only URL for evidence (origin + pathname, no query/hash/credentials).
 * @param {string} url
 * @returns {string}
 */
export function sanitizeUrl(url) {
  if (!url) return "";
  try {
    const u = new URL(url);
    return `${u.origin}${u.pathname}`;
  } catch {
    return sanitizeText(String(url).split("?")[0].split("#")[0]);
  }
}

/**
 * Cap and sanitize diagnostic lines for evidence (never raw browser dumps).
 * @param {string[]} lines
 * @param {string[]} [secrets]
 * @param {number} [max]
 * @returns {string[]}
 */
export function sanitizeLogLines(lines, secrets = [], max = 20) {
  return (lines || [])
    .slice(0, max)
    .map((l) => sanitizeText(l, secrets).slice(0, 240));
}
