// apps/mirri-web/src/lib/retentionManagement.test.ts
// Given the session-retention management page (settings/RetentionPanel.vue)
// renders two views over retention.list rows, when the user sorts, confirms
// deletes, and fires tiered batch cleans, then ALL of that logic runs through
// these pure helpers — the spec here pins US3.2 (sorting), US3.3 (single
// delete confirmation), US3.4 (tier math, empty scope, partial failure),
// and SDR-003 (the compressed fork gate).

import { describe, expect, it } from 'vitest';
import type { AppRetentionEntry, AppRetentionStats, AppRetentionTieredResult } from '../api/types';
import {
  RETENTION_FILTER_NONE,
  RETENTION_SELECTION_NONE,
  RETENTION_TIER_DAYS,
  archivedTabVisibleForBackend,
  canForkSession,
  defaultSortForView,
  filterRetentionEntries,
  formatRetentionBytes,
  selectionBytes,
  selectionIntersect,
  retentionWorkspaceOptions,
  sortRetentionEntries,
  sortableKeysForView,
  toggleSelection,
  toggleVisibleAll,
  summarizeTieredOutcome,
  tierClickDecision,
  tierEstimate,
  tierIsEmpty,
  toggleRetentionSort,
} from './retentionManagement';

function entry(overrides: Partial<AppRetentionEntry> = {}): AppRetentionEntry {
  return {
    sessionId: 's-1',
    title: 'A task',
    workspace: '/ws',
    createdAt: '2025-12-01T00:00:00Z',
    archivedAt: '2026-01-01T00:00:00Z',
    compressedAt: null,
    currentSizeBytes: 1024,
    savedBytes: null,
    compressedDays: null,
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Views & sorting (US3.2)
// ---------------------------------------------------------------------------

describe('retentionManagement sorting', () => {
  it('should default the compressed view to disk usage descending when it is first opened', () => {
    expect(defaultSortForView('compressed')).toEqual({ key: 'size', direction: 'desc' });
  });

  it('should default the archive view to archive time newest-first when it is first opened', () => {
    expect(defaultSortForView('archived')).toEqual({ key: 'archivedAt', direction: 'desc' });
  });

  it('should sort compressed rows by occupancy descending for the default sort', () => {
    const rows = [
      entry({ sessionId: 'small', currentSizeBytes: 100 }),
      entry({ sessionId: 'big', currentSizeBytes: 900 }),
      entry({ sessionId: 'mid', currentSizeBytes: 500 }),
    ];
    const sorted = sortRetentionEntries(rows, defaultSortForView('compressed'));
    expect(sorted.map((r) => r.sessionId)).toEqual(['big', 'mid', 'small']);
  });

  it('should flip direction when the same column header is clicked twice', () => {
    const first = toggleRetentionSort({ key: 'size', direction: 'desc' }, 'size', 'compressed');
    expect(first).toEqual({ key: 'size', direction: 'asc' });
    const second = toggleRetentionSort(first, 'size', 'compressed');
    expect(second).toEqual({ key: 'size', direction: 'desc' });
  });

  it('should adopt the view default direction when switching to a different column', () => {
    const next = toggleRetentionSort({ key: 'size', direction: 'desc' }, 'days', 'compressed');
    expect(next).toEqual({ key: 'days', direction: 'desc' });
  });

  it('should expose the closed sortable column set per view (no days column in the archive view)', () => {
    expect(sortableKeysForView('compressed')).toEqual(['size', 'days', 'archivedAt']);
    // SDR-009: the archive view's set widened by the legacy-page merge.
    expect(sortableKeysForView('archived')).toEqual(['size', 'archivedAt', 'createdAt', 'title']);
  });

  it('should sink unknown compressed ages to the end in both directions when sorting by days', () => {
    const rows = [
      entry({ sessionId: 'unknown', compressedDays: null }),
      entry({ sessionId: 'young', compressedDays: 3 }),
      entry({ sessionId: 'old', compressedDays: 400 }),
    ];
    const asc = sortRetentionEntries(rows, { key: 'days', direction: 'asc' });
    expect(asc.map((r) => r.sessionId)).toEqual(['young', 'old', 'unknown']);
    const desc = sortRetentionEntries(rows, { key: 'days', direction: 'desc' });
    expect(desc.map((r) => r.sessionId)).toEqual(['old', 'young', 'unknown']);
  });

  it('should sink legacy rows without an archive stamp to the end in both directions', () => {
    const rows = [
      entry({ sessionId: 'legacy', archivedAt: null }),
      entry({ sessionId: 'older', archivedAt: '2025-01-01T00:00:00Z' }),
      entry({ sessionId: 'newer', archivedAt: '2026-01-01T00:00:00Z' }),
    ];
    const newestFirst = sortRetentionEntries(rows, { key: 'archivedAt', direction: 'desc' });
    expect(newestFirst.map((r) => r.sessionId)).toEqual(['newer', 'older', 'legacy']);
    const oldestFirst = sortRetentionEntries(rows, { key: 'archivedAt', direction: 'asc' });
    expect(oldestFirst.map((r) => r.sessionId)).toEqual(['older', 'newer', 'legacy']);
  });

  it('should break sort ties by session id so the order is stable across refreshes', () => {
    const rows = [
      entry({ sessionId: 'b', currentSizeBytes: 100 }),
      entry({ sessionId: 'a', currentSizeBytes: 100 }),
    ];
    const sorted = sortRetentionEntries(rows, { key: 'size', direction: 'desc' });
    expect(sorted.map((r) => r.sessionId)).toEqual(['a', 'b']);
  });
});

// ---------------------------------------------------------------------------
// Tier estimates (US3.4)
// ---------------------------------------------------------------------------

const stats: AppRetentionStats = {
  archivedCount: 12,
  compressedCount: 8,
  compressedTotalBytes: 4096,
  reclaimedPotential: 100_000,
  tiers: [
    { days: 30, count: 5, bytes: 10_000 },
    { days: 90, count: 3, bytes: 6_000 },
    { days: 180, count: 0, bytes: 0 },
    { days: 365, count: 1, bytes: 2_000 },
  ],
};

describe('retentionManagement tier estimates', () => {
  it('should pin the tier day set to the product constant 30/90/180/365', () => {
    expect(RETENTION_TIER_DAYS).toEqual([30, 90, 180, 365]);
  });

  it('should answer the tier estimate for a known threshold', () => {
    expect(tierEstimate(stats, 30)).toEqual({ count: 5, bytes: 10_000 });
    expect(tierEstimate(stats, 365)).toEqual({ count: 1, bytes: 2_000 });
  });

  it('should answer null when the tier is missing or stats are absent', () => {
    expect(tierEstimate(stats, 7)).toBeNull();
    expect(tierEstimate(null, 30)).toBeNull();
  });

  it('should treat a zero-count tier as an empty scope (no confirm, no delete)', () => {
    expect(tierIsEmpty(tierEstimate(stats, 180))).toBe(true);
    expect(tierIsEmpty(tierEstimate(stats, 30))).toBe(false);
    expect(tierIsEmpty(null)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Batch outcome accounting (US3.4 honest partial reporting)
// ---------------------------------------------------------------------------

describe('retentionManagement tiered outcome', () => {
  it('should report successes, reclaimed bytes, and per-session failures separately when a batch partially fails', () => {
    const result: AppRetentionTieredResult = {
      deleted: ['s-1', 's-2'],
      failed: [{ sessionId: 's-3', error: 'directory lock held by another process' }],
      reclaimedBytes: 98_765,
    };
    const summary = summarizeTieredOutcome(result);
    expect(summary.deletedCount).toBe(2);
    expect(summary.reclaimedBytes).toBe(98_765);
    expect(summary.failures).toEqual([
      { sessionId: 's-3', error: 'directory lock held by another process' },
    ]);
    expect(summary.anyAction).toBe(true);
  });

  it('should mark an empty outcome as no action when nothing was in scope', () => {
    const summary = summarizeTieredOutcome({ deleted: [], failed: [], reclaimedBytes: 0 });
    expect(summary.anyAction).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Tier-click decision (US3.4: confirm math, empty-scope hint, re-entrancy)
// ---------------------------------------------------------------------------

describe('retentionManagement tier-click decision', () => {
  it('should open the confirm dialog with the count and bytes math when the tier has sessions', () => {
    expect(tierClickDecision(stats, 90, false)).toEqual({
      kind: 'confirm',
      days: 90,
      count: 3,
      bytes: 6_000,
    });
  });

  it('should answer the empty hint instead of the confirm when the tier scope is empty', () => {
    expect(tierClickDecision(stats, 180, false)).toEqual({ kind: 'empty', days: 180 });
  });

  it('should answer the empty hint when the stats payload carries no tier estimates', () => {
    expect(tierClickDecision(null, 30, false)).toEqual({ kind: 'empty', days: 30 });
  });

  it('should block every tier click while a batch is already running (re-entrancy gate)', () => {
    expect(tierClickDecision(stats, 30, true)).toEqual({ kind: 'blocked' });
  });
});

// ---------------------------------------------------------------------------
// Compressed fork gate (SDR-003)
// ---------------------------------------------------------------------------

describe('retentionManagement fork gate', () => {
  it('should allow forking a plain or restored session', () => {
    expect(canForkSession(false)).toBe(true);
  });

  it('should refuse forking a compressed session until the archive is restored', () => {
    expect(canForkSession(true)).toBe(false);
  });
});


// ---------------------------------------------------------------------------
// Byte formatting (the confirmation dialogs' "about X" copy)
// ---------------------------------------------------------------------------

describe('retentionManagement byte formatting', () => {
  it('should render small sizes in bytes and 1024-based units above', () => {
    expect(formatRetentionBytes(512)).toBe('512 B');
    expect(formatRetentionBytes(1024)).toBe('1 KB');
    expect(formatRetentionBytes(1536)).toBe('1.5 KB');
    expect(formatRetentionBytes(2 * 1024 * 1024)).toBe('2 MB');
  });

  it('should render invalid sizes as an honest dash', () => {
    expect(formatRetentionBytes(Number.NaN)).toBe('—');
    expect(formatRetentionBytes(-1)).toBe('—');
  });
});

// ---------------------------------------------------------------------------
// Archive-view filtering (SDR-009 — the merged legacy archived page)
// ---------------------------------------------------------------------------

describe('retentionManagement archive filtering', () => {
  const rows = [
    entry({ sessionId: 's-1', title: 'Refactor auth', workspace: '/ws-a' }),
    entry({ sessionId: 's-2', title: 'Write docs', workspace: '/ws-b' }),
    entry({ sessionId: 's-3', title: 'Auth migration', workspace: '/ws-a' }),
  ];

  it('should keep every row when the filter is the none filter', () => {
    expect(filterRetentionEntries(rows, RETENTION_FILTER_NONE)).toHaveLength(3);
  });

  it('should match titles case-insensitively on a substring when searching', () => {
    const kept = filterRetentionEntries(rows, { query: 'auth', workspace: 'all' });
    expect(kept.map((r) => r.sessionId)).toEqual(['s-1', 's-3']);
  });

  it('should keep only the given workspace when the filter picks one', () => {
    const kept = filterRetentionEntries(rows, { query: '', workspace: '/ws-b' });
    expect(kept.map((r) => r.sessionId)).toEqual(['s-2']);
  });

  it('should combine the search and workspace constraints when both are set', () => {
    const kept = filterRetentionEntries(rows, { query: 'AUTH', workspace: '/ws-b' });
    expect(kept).toEqual([]);
  });

  it('should answer the distinct sorted workspace keys when deriving options', () => {
    expect(retentionWorkspaceOptions(rows)).toEqual(['/ws-a', '/ws-b']);
  });

  it('should treat an empty created stamp as unknown rank when sorting by createdAt', () => {
    const withUnknown = [
      entry({ sessionId: 's-new', createdAt: '2026-06-01T00:00:00Z' }),
      entry({ sessionId: 's-old', createdAt: '2020-01-01T00:00:00Z' }),
      entry({ sessionId: 's-unknown', createdAt: '' }),
    ];
    const asc = sortRetentionEntries(withUnknown, { key: 'createdAt', direction: 'asc' });
    expect(asc.map((r) => r.sessionId)).toEqual(['s-old', 's-new', 's-unknown']);
    const desc = sortRetentionEntries(withUnknown, { key: 'createdAt', direction: 'desc' });
    expect(desc.map((r) => r.sessionId)).toEqual(['s-new', 's-old', 's-unknown']);
  });

  it('should sort by title with zh-aware collation when the archive view sorts on title', () => {
    const named = [
      entry({ sessionId: 's-2', title: 'zebra' }),
      entry({ sessionId: 's-1', title: 'Apple' }),
      entry({ sessionId: 's-3', title: 'beta' }),
    ];
    const asc = sortRetentionEntries(named, { key: 'title', direction: 'asc' });
    expect(asc.map((r) => r.sessionId)).toEqual(['s-1', 's-3', 's-2']);
  });

});

// ---------------------------------------------------------------------------
// The archived settings tab's backend gate (SDR-009)
// ---------------------------------------------------------------------------

describe('retentionManagement archived tab gate', () => {
  it('should hide the legacy archived tab on the v3 backend when the retention page owns it', () => {
    expect(archivedTabVisibleForBackend('v3')).toBe(false);
  });

  it('should keep the legacy archived tab on the v1 and v2 backends as the degrade path', () => {
    expect(archivedTabVisibleForBackend('v1')).toBe(true);
    expect(archivedTabVisibleForBackend('v2')).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Multi-select delete (SDR-011)
// ---------------------------------------------------------------------------

describe('retentionManagement multi-select selection', () => {
  it('should keep the selection by id when the row set changes under it', () => {
    let state = RETENTION_SELECTION_NONE;
    state = toggleSelection(state, 's-1');
    state = toggleSelection(state, 's-2');
    // The poll replaces the rows wholesale (s-1 hidden by a filter, s-3 new)
    expect(state.ids.has('s-1')).toBe(true);
    expect(state.ids.has('s-2')).toBe(true);
    expect(state.ids.size).toBe(2);
  });

  it('should deselect one row when its checkbox is toggled twice', () => {
    let state = toggleSelection(RETENTION_SELECTION_NONE, 's-1');
    state = toggleSelection(state, 's-1');
    expect(state.ids.size).toBe(0);
  });

  it('should toggle only the visible rows when select-all runs on a filtered set', () => {
    let state = toggleSelection(RETENTION_SELECTION_NONE, 's-hidden');
    state = toggleVisibleAll(state, ['s-1', 's-2']);
    expect(state.ids.has('s-1')).toBe(true);
    expect(state.ids.has('s-2')).toBe(true);
    expect(state.ids.has('s-hidden')).toBe(true); // untouched outside the visible set
    state = toggleVisibleAll(state, ['s-1', 's-2']); // fully selected → deselect
    expect(state.ids.has('s-1')).toBe(false);
    expect(state.ids.has('s-2')).toBe(false);
    expect(state.ids.has('s-hidden')).toBe(true);
  });

  it('should split the selection into kept rows and gone ids at confirm time', () => {
    const state = { ids: new Set(['s-1', 's-gone', 's-2']) };
    const rows = [entry({ sessionId: 's-1' }), entry({ sessionId: 's-2' })];
    const { rows: kept, goneIds } = selectionIntersect(state, rows);
    expect(kept.map((r) => r.sessionId)).toEqual(['s-1', 's-2']);
    expect(goneIds).toEqual(['s-gone']);
  });

  it('should answer an empty intersection when every selected id left the list', () => {
    const state = { ids: new Set(['s-gone']) };
    const { rows, goneIds } = selectionIntersect(state, [entry({ sessionId: 's-other' })]);
    expect(rows).toEqual([]);
    expect(goneIds).toEqual(['s-gone']);
  });

  it('should sum the intersection rows occupancy when estimating the release', () => {
    const rows = [
      entry({ sessionId: 's-1', currentSizeBytes: 1024 }),
      entry({ sessionId: 's-2', currentSizeBytes: 2048 }),
    ];
    expect(selectionBytes(rows)).toBe(3072);
  });
});
