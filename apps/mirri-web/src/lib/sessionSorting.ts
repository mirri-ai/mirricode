// apps/mirri-web/src/lib/sessionSorting.ts
// SDR-002: the sidebar's sort projection. Position depends ONLY on
// {id, title, lastUsedAt, updatedAt, pinned, pinnedAt} — never on the
// work-facts tuple or usage, so badge transitions can churn without
// touching the sort derivation (and vice versa).
// SDR-016: the pinned group rides the top — within it, the latest pin
// moment wins; everything else keeps the recency order.
// SDR-020: pinning is a GLOBAL statement, so the pinned rows leave their
// workspace group and ride one cross-workspace section at the top of the
// sidebar (see splitPinnedSessions). The comparator below keeps its pinned
// branch because the mobile switcher still renders pinned rows inline.

export interface SortableSessionRow {
  id: string;
  title: string;
  lastUsedAt: string | undefined;
  updatedAt: string;
  /** SDR-016: the pinned group's membership. */
  pinned: boolean;
  /** SDR-016: the pin moment — the pinned group's inner sort key. */
  pinnedAt: string | undefined;
  /** SDR-016/017: the pin's color (display-only — never a sort input). */
  pinColor?: 'red' | 'orange' | 'yellow' | 'green' | 'blue' | 'purple' | 'gray';
}

export function projectSessionForSort(session: {
  id: string;
  title: string;
  lastUsedAt?: string;
  updatedAt: string;
  pinned?: boolean;
  pinnedAt?: string;
  pinColor?: 'red' | 'orange' | 'yellow' | 'green' | 'blue' | 'purple' | 'gray';
}): SortableSessionRow {
  return {
    id: session.id,
    title: session.title,
    lastUsedAt: session.lastUsedAt,
    updatedAt: session.updatedAt,
    pinned: session.pinned === true,
    pinnedAt: session.pinnedAt,
    pinColor: session.pinColor,
  };
}

export function sortKeyEpoch(session: {
  lastUsedAt?: string;
  updatedAt: string;
}): number {
  return Date.parse(session.lastUsedAt ?? session.updatedAt) || 0;
}

/**
 * SDR-016/019: the pin moment comparator — the latest pin wins. Shared by
 * the per-workspace comparator and the global pinned section so both read
 * the same rule from one place.
 */
export function comparePinnedMoment(
  a: { pinnedAt: string | undefined },
  b: { pinnedAt: string | undefined },
): number {
  return (Date.parse(b.pinnedAt ?? '') || 0) - (Date.parse(a.pinnedAt ?? '') || 0);
}

/**
 * SDR-020: split the sidebar projection into the global pinned section and
 * the remainder. The pinned half is ordered by pin moment (descending), so
 * the last pin reads at the top of the section; the remainder is returned in
 * input order — the per-workspace ordering stays sortSessionsByUse's job.
 *
 * Pure and position-only: it reads `pinned` / `pinnedAt` and nothing else,
 * so work-facts churn can never re-order the section.
 */
export function splitPinnedSessions<T extends SortableSessionRow>(
  rows: readonly T[],
): { pinned: T[]; rest: T[] } {
  const pinned: T[] = [];
  const rest: T[] = [];
  for (const row of rows) (row.pinned ? pinned : rest).push(row);
  return { pinned: pinned.toSorted(comparePinnedMoment), rest };
}

export function sortSessionsByUse<T extends SortableSessionRow>(
  rows: readonly T[],
): T[] {
  return rows.toSorted((a, b) => {
    // The pinned group rides the top (SDR-016); within it, the latest pin
    // moment wins; the unpinned rows keep the recency order.
    if (a.pinned !== b.pinned) {
      return a.pinned ? -1 : 1;
    }
    if (a.pinned && b.pinned) {
      return comparePinnedMoment(a, b);
    }
    return sortKeyEpoch(b) - sortKeyEpoch(a);
  });
}
