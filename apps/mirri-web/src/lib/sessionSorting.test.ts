// apps/mirri-web/src/lib/sessionSorting.test.ts
// Sidebar sort projection (SDR-002 position-state decoupling): position
// derives ONLY from {id, title, lastUsedAt, updatedAt} — never from the
// work-facts tuple or usage.

import { describe, expect, it } from 'vitest';
import {
  sortSessionsByUse,
  projectSessionForSort,
  sortKeyEpoch,
  splitPinnedSessions,
} from './sessionSorting';

describe('sidebar sort projection (SDR-002 position-state decoupling)', () => {
  it('should order by lastUsedAt falling back to updatedAt', () => {
    const rows = [
      projectSessionForSort({ id: 'a', title: 'a', lastUsedAt: '2026-01-01T00:00:00Z', updatedAt: '2026-01-01T00:00:00Z' }),
      projectSessionForSort({ id: 'b', title: 'b', lastUsedAt: '2026-03-01T00:00:00Z', updatedAt: '2026-01-01T00:00:00Z' }),
      projectSessionForSort({ id: 'c', title: 'c', lastUsedAt: undefined, updatedAt: '2026-02-01T00:00:00Z' }),
    ];
    const sorted = sortSessionsByUse(rows);
    expect(sorted.map((r) => r.id)).toEqual(['b', 'c', 'a']);
  });

  it('should expose only the sort projection fields', () => {
    const projected = projectSessionForSort({
      id: 'a',
      title: 'a',
      lastUsedAt: '2026-01-01T00:00:00Z',
      updatedAt: '2026-01-02T00:00:00Z',
      // Work-facts noise that must NOT leak into the projection.
      busy: true,
      pendingInteraction: 'approval',
    } as Parameters<typeof projectSessionForSort>[0]);
    expect(Object.keys(projected).toSorted()).toEqual(['id', 'lastUsedAt', 'pinColor', 'pinned', 'pinnedAt', 'title', 'updatedAt']);
  });

  it('should answer 0 for unparseable timestamps', () => {
    expect(
      sortKeyEpoch({ lastUsedAt: undefined, updatedAt: '' }),
    ).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// SDR-016/017: the pinned group
// ---------------------------------------------------------------------------

describe('sessionSorting pinned group (SDR-016)', () => {
  const row = (
    id: string,
    opts: { pinned?: boolean; pinnedAt?: string; lastUsedAt?: string; updatedAt?: string } = {},
  ) => ({
    id,
    title: id,
    lastUsedAt: opts.lastUsedAt,
    updatedAt: opts.updatedAt ?? '2025-01-01T00:00:00Z',
    pinned: opts.pinned ?? false,
    pinnedAt: opts.pinnedAt,
  });

  it('places pinned rows above unpinned ones regardless of recency', () => {
    const sorted = sortSessionsByUse([
      row('new-unpinned', { lastUsedAt: '2099-01-01T00:00:00Z' }),
      row('old-pinned', { pinned: true, pinnedAt: '2020-01-01T00:00:00Z', lastUsedAt: '2020-01-01T00:00:00Z' }),
    ]);
    expect(sorted.map((r) => r.id)).toEqual(['old-pinned', 'new-unpinned']);
  });

  it('sorts the pinned group by pinnedAt descending', () => {
    const sorted = sortSessionsByUse([
      row('pinned-older', { pinned: true, pinnedAt: '2024-01-01T00:00:00Z' }),
      row('pinned-newer', { pinned: true, pinnedAt: '2025-01-01T00:00:00Z' }),
    ]);
    expect(sorted.map((r) => r.id)).toEqual(['pinned-newer', 'pinned-older']);
  });

  it('keeps the recency order among unpinned rows', () => {
    const sorted = sortSessionsByUse([
      row('stale', { lastUsedAt: '2024-01-01T00:00:00Z' }),
      row('fresh', { lastUsedAt: '2025-01-01T00:00:00Z' }),
    ]);
    expect(sorted.map((r) => r.id)).toEqual(['fresh', 'stale']);
  });
});

// ---------------------------------------------------------------------------
// SDR-020: the global pinned section
// ---------------------------------------------------------------------------

describe('sessionSorting global pinned split (SDR-020)', () => {
  const row = (
    id: string,
    opts: { pinned?: boolean; pinnedAt?: string; lastUsedAt?: string; updatedAt?: string } = {},
  ) => ({
    id,
    title: id,
    lastUsedAt: opts.lastUsedAt,
    updatedAt: opts.updatedAt ?? '2025-01-01T00:00:00Z',
    pinned: opts.pinned ?? false,
    pinnedAt: opts.pinnedAt,
  });

  it('should collect every pinned row into the pinned list and leave the rest untouched', () => {
    const { pinned, rest } = splitPinnedSessions([
      row('plain-a', { lastUsedAt: '2024-01-01T00:00:00Z' }),
      row('pin-a', { pinned: true, pinnedAt: '2025-01-01T00:00:00Z' }),
      row('plain-b', { lastUsedAt: '2025-06-01T00:00:00Z' }),
      row('pin-b', { pinned: true, pinnedAt: '2026-01-01T00:00:00Z' }),
    ]);

    expect(pinned.map((r) => r.id)).toEqual(['pin-b', 'pin-a']);
    // 其余行保持输入顺序 —— 分流不改写它的排序语义（组内排序仍由
    // sortSessionsByUse 负责）。
    expect(rest.map((r) => r.id)).toEqual(['plain-a', 'plain-b']);
  });

  it('should order the pinned list by pin moment descending across workspaces', () => {
    const { pinned } = splitPinnedSessions([
      row('old', { pinned: true, pinnedAt: '2020-01-01T00:00:00Z' }),
      row('newest', { pinned: true, pinnedAt: '2030-01-01T00:00:00Z' }),
      row('middle', { pinned: true, pinnedAt: '2025-01-01T00:00:00Z' }),
    ]);

    expect(pinned.map((r) => r.id)).toEqual(['newest', 'middle', 'old']);
  });

  it('should return an empty pinned list when nothing is pinned', () => {
    const { pinned, rest } = splitPinnedSessions([
      row('plain-a'),
      row('plain-b'),
    ]);

    expect(pinned).toEqual([]);
    expect(rest.map((r) => r.id)).toEqual(['plain-a', 'plain-b']);
  });
});
