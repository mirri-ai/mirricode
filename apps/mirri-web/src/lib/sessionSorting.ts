// apps/mirri-web/src/lib/sessionSorting.ts
// SDR-002: the sidebar's sort projection. Position depends ONLY on
// {id, title, lastUsedAt, updatedAt} — never on the work-facts tuple or
// usage, so badge transitions can churn without touching the sort
// derivation (and vice versa).

export interface SortableSessionRow {
  id: string;
  title: string;
  lastUsedAt: string | undefined;
  updatedAt: string;
}

export function projectSessionForSort(session: {
  id: string;
  title: string;
  lastUsedAt?: string;
  updatedAt: string;
}): SortableSessionRow {
  return {
    id: session.id,
    title: session.title,
    lastUsedAt: session.lastUsedAt,
    updatedAt: session.updatedAt,
  };
}

export function sortKeyEpoch(session: {
  lastUsedAt?: string;
  updatedAt: string;
}): number {
  return Date.parse(session.lastUsedAt ?? session.updatedAt) || 0;
}

export function sortSessionsByUse(rows: SortableSessionRow[]): SortableSessionRow[] {
  return rows.toSorted((a, b) => sortKeyEpoch(b) - sortKeyEpoch(a));
}
