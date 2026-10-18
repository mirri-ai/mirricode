// apps/mirri-web/src/lib/cronManagement.test.ts
// Pure view-logic coverage for the cron panel (B6): substring filtering over
// the row's identity fields, next-fire ordering with unscheduled rows sunk,
// and the due-state bucket the row badge renders from.

import { describe, expect, it } from 'vitest';
import { cronTaskDueState, filterCronTasks, sortCronTasks } from './cronManagement';
import type { AppCronTask } from '../api/types';

function task(overrides: Partial<AppCronTask> = {}): AppCronTask {
  return {
    id: 'cron-1', cron: '*/5 * * * *', prompt: 'tick', recurring: true,
    stale: false, createdAt: 1, lastFiredAt: null, nextFireAt: 100, ...overrides,
  };
}

describe('filterCronTasks', () => {
  it('matches on cron expression, prompt, and id, case-insensitively', () => {
    const rows = [
      task({ id: 'cron-1', cron: '*/5 * * * *', prompt: 'water the plants' }),
      task({ id: 'cron-2', cron: '0 9 * * *', prompt: 'daily briefing' }),
    ];
    expect(filterCronTasks(rows, 'brief').map((r) => r.id)).toEqual(['cron-2']);
    expect(filterCronTasks(rows, '*/5').map((r) => r.id)).toEqual(['cron-1']);
    expect(filterCronTasks(rows, 'CRON-2').map((r) => r.id)).toEqual(['cron-2']);
  });

  it('returns every row sorted when the query is blank', () => {
    const rows = [task({ id: 'b', nextFireAt: 200 }), task({ id: 'a', nextFireAt: 100 })];
    expect(filterCronTasks(rows, '').map((r) => r.id)).toEqual(['a', 'b']);
  });
});

describe('sortCronTasks', () => {
  it('orders by next fire ascending and sinks unscheduled rows to the end', () => {
    const rows = [
      task({ id: 'fired', nextFireAt: undefined }),
      task({ id: 'later', nextFireAt: 300 }),
      task({ id: 'sooner', nextFireAt: 100 }),
    ];
    expect(sortCronTasks(rows).map((r) => r.id)).toEqual(['sooner', 'later', 'fired']);
  });

  it('breaks ties by id so the order is stable across refreshes', () => {
    const rows = [task({ id: 'b', nextFireAt: 100 }), task({ id: 'a', nextFireAt: 100 })];
    expect(sortCronTasks(rows).map((r) => r.id)).toEqual(['a', 'b']);
  });
});

describe('cronTaskDueState', () => {
  it('buckets by the next fire relative to now', () => {
    expect(cronTaskDueState(50, 100)).toBe('due');
    expect(cronTaskDueState(150, 100)).toBe('future');
    expect(cronTaskDueState(null, 100)).toBe('dormant');
    expect(cronTaskDueState(undefined, 100)).toBe('dormant');
  });
});
