// apps/mirri-web/src/lib/sessionSorting.ts
// SDR-002: the sidebar's sort projection. Position depends ONLY on
// {id, title, lastUsedAt, updatedAt, pinned, pinnedAt} — never on the
// work-facts tuple or usage, so badge transitions can churn without
// touching the sort derivation (and vice versa).
// SDR-016: the pinned group rides the top — within it, the latest pin
// moment wins; everything else keeps the recency order.

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

export function sortSessionsByUse(rows: SortableSessionRow[]): SortableSessionRow[] {
  return rows.toSorted((a, b) => {
    // The pinned group rides the top (SDR-016); within it, the latest pin
    // moment wins; the unpinned rows keep the recency order.
    if (a.pinned !== b.pinned) {
      return a.pinned ? -1 : 1;
    }
    if (a.pinned && b.pinned) {
      return (Date.parse(b.pinnedAt ?? '') || 0) - (Date.parse(a.pinnedAt ?? '') || 0);
    }
    return sortKeyEpoch(b) - sortKeyEpoch(a);
  });
}
