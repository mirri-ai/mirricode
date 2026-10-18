// apps/mirri-web/src/lib/cronManagement.ts
// Pure TS — no Vue. Cron-panel view logic (B6): filtering, next-fire
// ordering, and the due-state bucket the row badge renders from.

import type { AppCronTask } from '../api/types';

/** Case-insensitive substring filter on cron expression / prompt / id. */
export function filterCronTasks(
  tasks: ReadonlyArray<AppCronTask>,
  query: string,
): AppCronTask[] {
  const q = query.trim().toLowerCase();
  const rows = q
    ? tasks.filter(
        (t) =>
          t.cron.toLowerCase().includes(q) ||
          t.prompt.toLowerCase().includes(q) ||
          t.id.toLowerCase().includes(q),
      )
    : [...tasks];
  return sortCronTasks(rows);
}

/** Next-fire ascending; tasks without a schedule (fired one-shots) sink to
 *  the end, id breaking ties so the order is stable across refreshes. */
export function sortCronTasks(tasks: ReadonlyArray<AppCronTask>): AppCronTask[] {
  return [...tasks].toSorted((a, b) => {
    const an = a.nextFireAt ?? Number.POSITIVE_INFINITY;
    const bn = b.nextFireAt ?? Number.POSITIVE_INFINITY;
    if (an !== bn) return an - bn;
    return a.id.localeCompare(b.id);
  });
}

/** The row badge bucket: a scheduled future fire, an overdue fire (the
 *  driver's next tick delivers it), or a fired one-shot at rest. */
export function cronTaskDueState(
  nextFireAt: number | null | undefined,
  now: number,
): 'future' | 'due' | 'dormant' {
  if (nextFireAt === null || nextFireAt === undefined) return 'dormant';
  return nextFireAt <= now ? 'due' : 'future';
}
