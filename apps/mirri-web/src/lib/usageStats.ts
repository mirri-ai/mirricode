export interface UsageBuckets {
  inputTokens: number; // 未缓存输入（排他口径）
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
}

/** The prompt-side billing denominator (SDR-001): the three disjoint
 *  input buckets. Output is never part of it. */
export function billedInputTokens(u: UsageBuckets): number {
  return u.inputTokens + u.cacheReadTokens + u.cacheCreationTokens;
}

/** Anti-fake-full-hit cache hit percentage (SDR-001 d / SDR-004): a partial
 *  hit never displays as 100 — precision deepens until the shown value stays
 *  below 100. Session-level callers get two decimal places (SDR-004);
 *  turn-level callers pass their own precision (1, per US3.3). */
export function formatCacheHitPercent(cacheRead: number, billedInput: number, decimalPlaces = 2): string | null {
  if (billedInput <= 0) return null;
  const ratio = cacheRead / billedInput;
  if (ratio >= 1) return '100';
  for (let dp = decimalPlaces; dp <= 6; dp += 1) {
    const shown = Number((ratio * 100).toFixed(dp));
    if (shown < 100) return shown.toFixed(dp);
  }
  return (100 - Math.pow(10, -6)).toFixed(6);
}

export function formatExactTokens(n: number): string {
  return n.toLocaleString('en-US');
}