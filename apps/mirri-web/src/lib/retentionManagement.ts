// apps/mirri-web/src/lib/retentionManagement.ts
// Pure TS — no Vue. The archived/compressed management-page view logic
// (session-retention Task 11): sort keys, tier-estimate lookups, tier
// confirmation math, and the honest partial-failure summary. The panel
// (settings/RetentionPanel.vue) stays a thin renderer over these.

import type {
  AppRetentionEntry,
  AppRetentionStats,
  AppRetentionTieredResult,
} from '../api/types';

/** The product's fixed delete-tier day set (the architecture's product
 *  constant — NOT configuration, SDR-002). */
export const RETENTION_TIER_DAYS: readonly number[] = [30, 90, 180, 365];

/** Sortable column keys — the closed set of the two views (US3.2). */
export type RetentionSortKey = 'size' | 'days' | 'archivedAt';

export interface RetentionSort {
  key: RetentionSortKey;
  direction: 'asc' | 'desc';
}

/** The default sort per view (US3.2): the compressed view opens by disk
 *  occupancy descending (find the biggest space hogs first), the archive
 *  view by archive time most-recent-first. */
export function defaultSortForView(view: 'archived' | 'compressed'): RetentionSort {
  return view === 'compressed'
    ? { key: 'size', direction: 'desc' }
    : { key: 'archivedAt', direction: 'desc' };
}

/** Sort the management rows. Null/missing day stamps sink to the end
 *  regardless of direction (an unknown age is never "the oldest"); the
 *  session id breaks ties so the order is stable across refreshes. */
export function sortRetentionEntries(
  rows: ReadonlyArray<AppRetentionEntry>,
  sort: RetentionSort,
): AppRetentionEntry[] {
  const factor = sort.direction === 'desc' ? -1 : 1;
  return [...rows].toSorted((a, b) => {
    const aUnknown = sortRankIsUnknown(a, sort.key);
    const bUnknown = sortRankIsUnknown(b, sort.key);
    // Unknown ranks sink to the end in BOTH directions (US3.2: a missing
    // stamp is never the most extreme row).
    if (aUnknown !== bUnknown) return aUnknown ? 1 : -1;
    if (aUnknown) return a.sessionId.localeCompare(b.sessionId);
    let cmp: number;
    switch (sort.key) {
      case 'size':
        cmp = a.currentSizeBytes - b.currentSizeBytes;
        break;
      case 'days':
        cmp = (a.compressedDays ?? 0) - (b.compressedDays ?? 0);
        break;
      case 'archivedAt':
        cmp = retentionStampRank(a) - retentionStampRank(b);
        break;
    }
    if (cmp !== 0) return cmp * factor;
    return a.sessionId.localeCompare(b.sessionId);
  });
}

/** Toggle the sort: same key flips direction; a different key adopts the
 *  key with its view default direction. */
export function toggleRetentionSort(
  current: RetentionSort,
  key: RetentionSortKey,
  view: 'archived' | 'compressed',
): RetentionSort {
  if (current.key === key) {
    return { key, direction: current.direction === 'asc' ? 'desc' : 'asc' };
  }
  return { key, direction: defaultSortForView(view).direction };
}

/** Whether a row's sort rank is unknown for the key (a null compressed age
 *  or a missing/unparseable archive stamp — legacy rows). */
function sortRankIsUnknown(row: AppRetentionEntry, key: RetentionSortKey): boolean {
  if (key === 'days') return row.compressedDays === null;
  if (key === 'archivedAt') {
    return row.archivedAt === null || Number.isNaN(Date.parse(row.archivedAt));
  }
  return false; // size is always known
}

/** archivedAt rank for sorting (a parse failure was filtered above). */
function retentionStampRank(row: AppRetentionEntry): number {
  const t = Date.parse(row.archivedAt ?? '');
  return Number.isNaN(t) ? 0 : t;
}

/** The sortable column set per view (US3.2's closed sets: the archive view
 *  has no compressed-age column). */
export function sortableKeysForView(view: 'archived' | 'compressed'): RetentionSortKey[] {
  return view === 'compressed' ? ['size', 'days', 'archivedAt'] : ['size', 'archivedAt'];
}

/** One tier button's confirmation math — the stats' tier estimate for the
 *  day threshold (null when the server's payload predates tier estimates
 *  or the tier is missing: the caller falls back to zero-scoped copy). */
export function tierEstimate(
  stats: AppRetentionStats | null,
  days: number,
): { count: number; bytes: number } | null {
  const row = stats?.tiers.find((tier) => tier.days === days);
  return row ? { count: row.count, bytes: row.bytes } : null;
}

/** Whether a tier button is a no-op worth the "no matching sessions" hint
 *  instead of the confirm dialog (US3.4: empty scope never executes). */
export function tierIsEmpty(estimate: { count: number } | null): boolean {
  return estimate === null || estimate.count === 0;
}

/** The batch outcome summarized for the result banner (US3.4's honest
 *  partial accounting): success line + per-failure lines, ready for i18n
 *  interpolation by the caller. */
export interface TieredOutcomeSummary {
  deletedCount: number;
  reclaimedBytes: number;
  failures: Array<{ sessionId: string; error: string }>;
  /** False when nothing was in scope at all (no banner at all). */
  anyAction: boolean;
}

export function summarizeTieredOutcome(result: AppRetentionTieredResult): TieredOutcomeSummary {
  return {
    deletedCount: result.deleted.length,
    reclaimedBytes: result.reclaimedBytes,
    failures: result.failed,
    anyAction: result.deleted.length > 0 || result.failed.length > 0,
  };
}

/** Human-readable byte size — 1024-based like the app's other byte
 *  formatting, one decimal, no trailing ".0" (512 / 1.5 KB / 2 MB / 1.2 GB). */
export function formatRetentionBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = -1;
  do {
    value /= 1024;
    unit += 1;
  } while (value >= 1024 && unit < units.length - 1);
  const text = value >= 100 || Number.isInteger(value)
    ? `${Math.round(value)}`
    : value.toFixed(1);
  return `${text} ${units[unit]}`;
}

// ---------------------------------------------------------------------------
// Decision logic — the panel's named pure exports (no component-test harness
// in this repo, so the decisions are testable as functions).
// ---------------------------------------------------------------------------

/** What a tier-button click should do (US3.4): with a non-empty estimate,
 *  open the confirm dialog with the count/bytes math; with an empty scope,
 *  show the "no matching sessions" hint — never a confirm, never a delete;
 *  while a batch runs, do nothing at all (re-entrancy gate). */
export type TierClickDecision =
  | { kind: 'confirm'; days: number; count: number; bytes: number }
  | { kind: 'empty'; days: number }
  | { kind: 'blocked' };

export function tierClickDecision(
  stats: AppRetentionStats | null,
  days: number,
  busy: boolean,
): TierClickDecision {
  if (busy) return { kind: 'blocked' };
  const estimate = tierEstimate(stats, days);
  if (estimate === null || estimate.count === 0) return { kind: 'empty', days };
  return { kind: 'confirm', days, count: estimate.count, bytes: estimate.bytes };
}

/** Whether the per-turn fork affordance exists for a session (SDR-003): a
 *  compressed session has no fork reality — the entries render disabled or
 *  hidden until the archive is restored. */
export function canForkSession(sessionCompressed: boolean): boolean {
  return !sessionCompressed;
}
