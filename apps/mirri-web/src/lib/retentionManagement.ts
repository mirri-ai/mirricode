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

/** Sortable column keys — the closed set of the two views (US3.2; the
 *  archive view's set widened by SDR-009 — createdAt/title joined from
 *  the merged legacy archived page). */
export type RetentionSortKey = 'size' | 'days' | 'archivedAt' | 'createdAt' | 'title';

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
      case 'createdAt':
        cmp = createdStampRank(a) - createdStampRank(b);
        break;
      case 'title':
        // zh-aware collation, same as the legacy archived page's name sort
        cmp = a.title.localeCompare(b.title, 'zh');
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

/** Whether a row's sort rank is unknown for the key (a null compressed age,
 *  a missing/unparseable archive stamp, or an empty created stamp — legacy
 *  or unreadable-state rows). */
function sortRankIsUnknown(row: AppRetentionEntry, key: RetentionSortKey): boolean {
  if (key === 'days') return row.compressedDays === null;
  if (key === 'archivedAt') {
    return row.archivedAt === null || Number.isNaN(Date.parse(row.archivedAt));
  }
  if (key === 'createdAt') {
    return row.createdAt === '' || Number.isNaN(Date.parse(row.createdAt));
  }
  return false; // size is always known; title always known
}

/** archivedAt rank for sorting (a parse failure was filtered above). */
function retentionStampRank(row: AppRetentionEntry): number {
  const t = Date.parse(row.archivedAt ?? '');
  return Number.isNaN(t) ? 0 : t;
}

/** createdAt rank for sorting (empty stamps were filtered above). */
function createdStampRank(row: AppRetentionEntry): number {
  const t = Date.parse(row.createdAt);
  return Number.isNaN(t) ? 0 : t;
}

/** The sortable column set per view (US3.2's closed sets: the archive view
 *  has no compressed-age column). */
export function sortableKeysForView(view: 'archived' | 'compressed'): RetentionSortKey[] {
  return view === 'compressed'
    ? ['size', 'days', 'archivedAt']
    : ['size', 'archivedAt', 'createdAt', 'title'];
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

// ---------------------------------------------------------------------------
// Archive-view filtering (SDR-009: the merged legacy archived page's search
// + workspace filter, client-side over the loaded rows — same shape the
// legacy page ran).
// ---------------------------------------------------------------------------

export interface RetentionFilter {
  /** Case-insensitive title substring; empty = no constraint. */
  query: string;
  /** The workspace bucket key to keep ('all' = no constraint). */
  workspace: string;
}

export const RETENTION_FILTER_NONE: RetentionFilter = { query: '', workspace: 'all' };

/** Filter the archive view's rows (ADR-010: the compressed view never
 *  filters — its decision dimensions are occupancy and age). */
export function filterRetentionEntries(
  rows: ReadonlyArray<AppRetentionEntry>,
  filter: RetentionFilter,
): AppRetentionEntry[] {
  const q = filter.query.trim().toLowerCase();
  return rows.filter((row) => {
    if (filter.workspace !== 'all' && row.workspace !== filter.workspace) return false;
    if (q && !row.title.toLowerCase().includes(q)) return false;
    return true;
  });
}

/** The distinct workspace bucket keys among the rows (sorted, for the
 *  filter dropdown's option list). */
export function retentionWorkspaceOptions(rows: ReadonlyArray<AppRetentionEntry>): string[] {
  const set = new Set<string>();
  for (const row of rows) set.add(row.workspace);
  return Array.from(set).toSorted((a, b) => a.localeCompare(b));
}

/** Whether a settings tab id is visible for the given backend (SDR-009:
 *  on v3 the archived tab merges into the retention management page; on
 *  v1/v2 the legacy archived tab stays as the degrade path). */
export function archivedTabVisibleForBackend(backend: 'v1' | 'v2' | 'v3'): boolean {
  return backend !== 'v3';
}

// ---------------------------------------------------------------------------
// Archive-view multi-select delete (SDR-011): the selection state and the
// confirm-time intersection math as pure functions.
// ---------------------------------------------------------------------------

/** The archive view's selection: the set of session ids the user checked.
 *  Keyed by id so the selection survives filter/sort/view switches and the
 *  30s poll's wholesale row replacement (SDR-011: the selection follows
 *  the session id, not the row). */
export interface RetentionSelectionState {
  ids: ReadonlySet<string>;
}

export const RETENTION_SELECTION_NONE: RetentionSelectionState = { ids: new Set() };

/** Toggle one row's checkbox. */
export function toggleSelection(
  state: RetentionSelectionState,
  id: string,
): RetentionSelectionState {
  const ids = new Set(state.ids);
  if (ids.has(id)) {
    ids.delete(id);
  } else {
    ids.add(id);
  }
  return { ids };
}

/** Toggle select-all over the currently VISIBLE rows (US3.3a: the
 *  select-all acts on the filtered visible set and is a toggle — a fully
 *  selected visible set deselects). Ids outside the visible set are left
 *  untouched. */
export function toggleVisibleAll(
  state: RetentionSelectionState,
  visibleIds: ReadonlyArray<string>,
): RetentionSelectionState {
  const visible = new Set(visibleIds);
  const allVisibleSelected = visibleIds.every((id) => state.ids.has(id));
  const ids = new Set(state.ids);
  if (allVisibleSelected) {
    for (const id of visible) ids.delete(id);
  } else {
    for (const id of visible) ids.add(id);
  }
  return { ids };
}

/** The confirm-time intersection (US3.3a criterion 3): which selected rows
 *  are still in the loaded archive list, and which selected ids left it
 *  (deleted/restored elsewhere, or moved to the compressed view by the
 *  auto-compressor). The row source is the UNFILTERED archive list — a
 *  filter-hidden selected id still counts. */
export function selectionIntersect(
  state: RetentionSelectionState,
  currentRows: ReadonlyArray<AppRetentionEntry>,
): { rows: AppRetentionEntry[]; goneIds: string[] } {
  const byId = new Map(currentRows.map((row) => [row.sessionId, row]));
  const rows: AppRetentionEntry[] = [];
  const goneIds: string[] = [];
  for (const id of state.ids) {
    const row = byId.get(id);
    if (row) rows.push(row);
    else goneIds.push(id);
  }
  return { rows, goneIds };
}

/** The confirm dialog's session-body release estimate: the intersection
 *  rows' current on-disk occupancy sum (the attachments' follow-up reclaim
 *  rides the confirmation copy, not this number). */
export function selectionBytes(rows: ReadonlyArray<AppRetentionEntry>): number {
  return rows.reduce((sum, row) => sum + row.currentSizeBytes, 0);
}
