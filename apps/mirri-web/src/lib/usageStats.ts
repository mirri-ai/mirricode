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

export type InputSegmentKey = 'cacheRead' | 'input' | 'cacheWrite';

export interface InputSegment {
  key: InputSegmentKey;
  tokens: number;
}

/** Split the three input buckets into priced-tier segments (ADR-004): the
 *  write segment does not exist when it is zero; read and uncached input are
 *  always present (a zero read is the real fact "never hit once"). */
export function splitInputBuckets(u: UsageBuckets): InputSegment[] {
  const segments: InputSegment[] = [
    { key: 'cacheRead', tokens: u.cacheReadTokens },
    { key: 'input', tokens: u.inputTokens },
  ];
  if (u.cacheCreationTokens > 0) {
    segments.push({ key: 'cacheWrite', tokens: u.cacheCreationTokens });
  }
  return segments;
}

/** One measured stats-bar group, for the width-fitting decision. */
export type StatsGroupKey =
  | 'turnCount'
  | 'cacheHit'
  | 'afterColdStart'
  | 'cacheRead'
  | 'input'
  | 'cacheWrite'
  | 'output';

export interface SizedSegment {
  key: StatsGroupKey;
  text: string;
  width: number;
}

/** One stats-bar group as built by the UI: its drop-priority key and its
 *  locale-formatted text. buildStatsGroups is the single source of truth
 *  for both which groups exist and which key each carries. */
export interface StatsGroup {
  key: StatsGroupKey;
  text: string;
}

/** Measured widths of the rendered groups, keyed by group key (never by
 *  index): a group dropped from the DOM keeps its last measured width, so
 *  dropping groups cannot pollute the next fitting decision. Partial by
 *  design — a group that was never rendered has no entry yet. */
export type GroupWidths = Partial<Record<StatsGroupKey, number>>;

const GROUP_KEYS: readonly StatsGroupKey[] = [
  'turnCount',
  'cacheHit',
  'afterColdStart',
  'cacheRead',
  'input',
  'cacheWrite',
  'output',
];

function isStatsGroupKey(value: string): value is StatsGroupKey {
  return (GROUP_KEYS as readonly string[]).includes(value);
}

/** Read the rendered `[data-group]` spans into a key→width record.
 *  Elements without a valid data-group key are ignored (e.g. separators);
 *  a group absent from `elements` keeps its previous measured width (0
 *  when never measured). Kept pure over any element-like list so jsdom can
 *  test it without a ResizeObserver. */
export function measureGroupWidths(
  elements: Iterable<{ dataset: { group?: string }; clientWidth: number }>,
  previous: GroupWidths,
): GroupWidths {
  const next = { ...previous };
  for (const el of elements) {
    const key = el.dataset.group;
    if (key !== undefined && isStatsGroupKey(key)) next[key] = el.clientWidth;
  }
  return next;
}

/** Drop order, lowest value first (ADR-004): turnCount → afterColdStart →
 *  output → cacheRead → cacheWrite; cacheHit and input are always kept. */
const DROP_ORDER: readonly StatsGroupKey[] = [
  'turnCount',
  'afterColdStart',
  'output',
  'cacheRead',
  'cacheWrite',
];

/** Decide which stats-bar groups fit the available width. Greedy: drop the
 *  lowest-value group (see DROP_ORDER) until the remaining widths fit. Width
 *  measurement is the caller's job — this function only decides. Always
 *  keeps `cacheHit` and `input` (with at least those two, the bar stays
 *  honest); if even they do not fit, they are still returned (they are the
 *  irreducible floor — the container clips rather than losing them). */
export function fitSegments(segments: SizedSegment[], availableWidth: number): SizedSegment[] {
  const kept = [...segments];
  const width = (list: SizedSegment[]): number => list.reduce((acc, s) => acc + s.width, 0);
  for (const key of DROP_ORDER) {
    if (width(kept) <= availableWidth) return kept;
    const idx = kept.findIndex((s) => s.key === key);
    if (idx >= 0) kept.splice(idx, 1);
  }
  return kept;
}
