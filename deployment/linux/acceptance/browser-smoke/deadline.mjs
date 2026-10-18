/**
 * Overall smoke deadline helpers. After expiry, steps must not keep running
 * with a floor timeout (the previous Math.max(5000, …) bug).
 */

/**
 * @param {number} deadlineMs absolute Date.now() deadline
 * @param {number} [now]
 * @returns {number} milliseconds remaining (> 0)
 */
export function remainingMs(deadlineMs, now = Date.now()) {
  const left = deadlineMs - now;
  if (left <= 0) {
    const err = new Error(`smoke deadline exceeded (budget exhausted)`);
    err.code = "SMOKE_DEADLINE";
    throw err;
  }
  return left;
}

/**
 * Cap a step timeout by remaining budget (no post-deadline floor).
 * @param {number} deadlineMs
 * @param {number} stepCapMs
 * @param {number} [now]
 */
export function stepTimeout(deadlineMs, stepCapMs, now = Date.now()) {
  return Math.min(stepCapMs, remainingMs(deadlineMs, now));
}
