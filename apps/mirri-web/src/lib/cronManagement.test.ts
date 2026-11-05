// apps/mirri-web/src/lib/cronManagement.test.ts
// Pure view-logic coverage for the scheduled-task page: the card list's
// status filter and ordering, the creation/edit draft's wire fold (A3/A4/
// A9), the prediction timeline (A5/B1/B2), and the pending-creation
// sentence (C3).

import { describe, expect, it } from 'vitest';

import {
  EMPTY_CRON_DRAFT,
  cronDraftRecurring,
  cronDraftSubmittable,
  cronDraftToInput,
  draftFromTask,
  filterScheduledTasks,
  isHighFrequencySchedule,
  lastRealRunAt,
  localDateOf,
  newCronDraft,
  predictionTimeline,
  rulePastInstant,
  scheduledTaskFilterCounts,
  scheduledTaskMatchesFilter,
  scheduledTaskRunCount,
  scheduledTaskStatus,
  zoneOffsetLabel,
  zonedDateOf,
} from './cronManagement';
import type { AppCronTask } from '../api/types';

function task(overrides: Partial<AppCronTask> = {}): AppCronTask {
  return {
    id: 'task-1',
    cron: '0 9 * * *',
    prompt: '回顾过去 24 小时的提交',
    recurring: true,
    stale: false,
    createdAt: 1,
    lastFiredAt: null,
    nextFireAt: 100,
    ...overrides,
  };
}

// --- the card list ---------------------------------------------------------

describe('scheduledTaskStatus', () => {
  it('Given an unpaused recurring task with budget left, When read, Then it is active', () => {
    expect(scheduledTaskStatus(task({ recurring: true }))).toBe('active');
  });

  it('Given a paused task, When read, Then it is paused regardless of its schedule', () => {
    expect(scheduledTaskStatus(task({ paused: true }))).toBe('paused');
  });

  it('Given a fired one-shot, When read, Then it is completed', () => {
    expect(scheduledTaskStatus(task({ recurring: false }))).toBe('completed');
  });

  it('Given an unbounded recurring task whose wire row carries maxFires null, When read, Then it is active (null means no cap, never a spent budget)', () => {
    // The cron.listAll row serialises `maxFires: null` for an uncapped task;
    // reading null as a number (0 >= null) once labelled every uncapped
    // agent-created task 「已结束」 the moment it was approved.
    const wireRow = task({ recurring: true, fireCount: 0 }) as unknown as Record<string, unknown>;
    wireRow['maxFires'] = null;
    expect(scheduledTaskStatus(wireRow as unknown as ReturnType<typeof task>)).toBe('active');
  });

  it('Given a recurring task that spent its budget, When read, Then it is completed', () => {
    expect(scheduledTaskStatus(task({ maxFires: 3, fireCount: 3 }))).toBe('completed');
  });

  it('Given a recurring task under its budget, When read, Then it stays active', () => {
    expect(scheduledTaskStatus(task({ maxFires: 3, fireCount: 2 }))).toBe('active');
  });
});

describe('scheduledTaskMatchesFilter', () => {
  it('Given the all tab, When checked, Then every task matches', () => {
    expect(scheduledTaskMatchesFilter(task(), 'all')).toBe(true);
    expect(scheduledTaskMatchesFilter(task({ paused: true }), 'all')).toBe(true);
  });

  it('Given the active and paused tabs, When checked, Then the paused flag decides', () => {
    expect(scheduledTaskMatchesFilter(task(), 'active')).toBe(true);
    expect(scheduledTaskMatchesFilter(task(), 'paused')).toBe(false);
    expect(scheduledTaskMatchesFilter(task({ paused: true }), 'active')).toBe(false);
    expect(scheduledTaskMatchesFilter(task({ paused: true }), 'paused')).toBe(true);
  });

  it('Given the failed tab, When checked, Then the caller-supplied last-run verdict decides', () => {
    expect(scheduledTaskMatchesFilter(task(), 'failed')).toBe(false);
    expect(scheduledTaskMatchesFilter(task(), 'failed', { lastRunFailed: true })).toBe(true);
    expect(
      scheduledTaskMatchesFilter(task(), 'failed', { lastRunFailed: false }),
    ).toBe(false);
  });
});

describe('filterScheduledTasks', () => {
  it('Given mixed statuses, When filtered to active, Then only active rows stay and the soonest comes first', () => {
    const rows = [
      task({ id: 'later', nextFireAt: 300 }),
      task({ id: 'paused', nextFireAt: 50, paused: true }),
      task({ id: 'sooner', nextFireAt: 100 }),
    ];
    expect(filterScheduledTasks(rows, 'active').map((row) => row.id)).toEqual([
      'sooner',
      'later',
    ]);
  });

  it('Given tasks without a next fire, When ordered, Then they sink to the end', () => {
    const rows = [
      task({ id: 'done', nextFireAt: null, recurring: false }),
      task({ id: 'next', nextFireAt: 100 }),
    ];
    expect(filterScheduledTasks(rows, 'all').map((row) => row.id)).toEqual(['next', 'done']);
  });

  it('Given the failed tab with run facts, When filtered, Then only the failing task stays', () => {
    const rows = [task({ id: 'a' }), task({ id: 'b', nextFireAt: 200 })];
    expect(
      filterScheduledTasks(rows, 'failed', { b: { lastRunFailed: true } }).map((row) => row.id),
    ).toEqual(['b']);
  });
});

describe('scheduledTaskFilterCounts', () => {
  it('Given a mixed list, When counted, Then each tab reads its own bucket', () => {
    const rows = [
      task({ id: 'a' }),
      task({ id: 'b', paused: true }),
      task({ id: 'c', recurring: false }),
    ];
    expect(scheduledTaskFilterCounts(rows, { a: { lastRunFailed: true } })).toEqual({
      all: 3,
      active: 1,
      paused: 1,
      failed: 1,
    });
  });
});

describe('scheduledTaskRunCount', () => {
  it('Given a task without a count, When read, Then it is zero', () => {
    expect(scheduledTaskRunCount({})).toBe(0);
  });

  it('Given a task with fires, When read, Then it is the delivered count', () => {
    expect(scheduledTaskRunCount({ fireCount: 24 })).toBe(24);
  });
});

// --- the creation / edit draft --------------------------------------------

describe('newCronDraft', () => {
  it('Given the page workspace, When drafted, Then the workspace is preset, the timezone is the local zone and the mode is manual (the session default)', () => {
    const draft = newCronDraft('ws-a');
    expect(draft.workspaceId).toBe('ws-a');
    expect(draft.timezone).not.toBe('');
    expect(draft.permissionMode).toBe('manual');
    expect(draft.schedule).toBeNull();
  });
});

describe('draftFromTask', () => {
  it('Given a task with an expression, When prefilled, Then the schedule opens in the advanced raw shape with its end date', () => {
    const draft = draftFromTask(task({ cron: '0 9 * * 1-5', endsAt: '2026-12-31' }));
    expect(draft.schedule).toEqual({
      kind: 'custom',
      mode: 'raw',
      raw: { expr: '0 9 * * 1-5' },
      ends: { kind: 'on', date: '2026-12-31' },
    });
  });

  it('Given a task with an interval, When prefilled, Then the interval shape rides and no expression is invented', () => {
    const draft = draftFromTask(
      task({
        cron: '',
        interval: { every: 2, unit: 'day', time: '09:00', anchorDate: '2026-11-01' },
      }),
    );
    expect(draft.schedule).toMatchObject({ kind: 'custom', mode: 'interval' });
    expect(draft.schedule).not.toHaveProperty('raw');
  });

  it('Given a task with every environment field, When prefilled, Then the draft mirrors them verbatim', () => {
    const draft = draftFromTask(
      task({
        title: '每日审查',
        model: 'claude-sonnet-4.5',
        effort: 'high',
        maxFires: 10,
        permissionMode: 'yolo',
        scope: { kind: 'workspace', workspaceId: 'ws-a' },
      }),
    );
    expect(draft.title).toBe('每日审查');
    expect(draft.model).toBe('claude-sonnet-4.5');
    expect(draft.effort).toBe('high');
    expect(draft.maxFiresText).toBe('10');
    expect(draft.permissionMode).toBe('yolo');
    expect(draft.workspaceId).toBe('ws-a');
  });

  it('Given a task without a permission mode, When prefilled, Then the selector defaults to manual', () => {
    expect(draftFromTask(task()).permissionMode).toBe('manual');
  });
});

describe('cronDraftSubmittable', () => {
  it('Given the empty draft, When checked, Then it is not submittable', () => {
    expect(cronDraftSubmittable(EMPTY_CRON_DRAFT)).toBe(false);
  });

  it('Given a schedule and a prompt, When checked, Then it is submittable even with every optional blank', () => {
    expect(
      cronDraftSubmittable({
        ...EMPTY_CRON_DRAFT,
        schedule: { kind: 'daily', time: '09:00' },
        prompt: 'shout',
      }),
    ).toBe(true);
  });

  it('Given a blank prompt, When checked, Then it is not submittable', () => {
    expect(
      cronDraftSubmittable({
        ...EMPTY_CRON_DRAFT,
        schedule: { kind: 'daily', time: '09:00' },
        prompt: '   ',
      }),
    ).toBe(false);
  });

  it('Given a structurally incomplete schedule, When checked, Then it is not submittable', () => {
    expect(
      cronDraftSubmittable({
        ...EMPTY_CRON_DRAFT,
        schedule: { kind: 'weekly', days: [], time: '09:00' },
        prompt: 'shout',
      }),
    ).toBe(false);
  });
});

describe('cronDraftToInput', () => {
  it('Given a workspace draft with a permission mode, When folded, Then the mode rides verbatim (A4: no posture fold)', () => {
    const input = cronDraftToInput({
      ...EMPTY_CRON_DRAFT,
      schedule: { kind: 'daily', time: '09:03' },
      prompt: 'review',
      timezone: 'Asia/Tokyo',
      permissionMode: 'auto',
      title: '每日审查',
      model: 'gpt-5',
      effort: 'high',
      workspaceId: 'ws-a',
    });
    expect(input).toEqual({
      cron: '3 9 * * *',
      prompt: 'review',
      recurring: true,
      timezone: 'Asia/Tokyo',
      permissionMode: 'auto',
      title: '每日审查',
      model: 'gpt-5',
      effort: 'high',
    });
  });

  it('Given an untouched environment, When folded, Then the optional keys stay absent and the default mode still rides', () => {
    const input = cronDraftToInput({
      ...EMPTY_CRON_DRAFT,
      schedule: { kind: 'daily', time: '09:00' },
      prompt: 'p',
      timezone: 'UTC',
    });
    expect(input).not.toHaveProperty('title');
    expect(input).not.toHaveProperty('model');
    expect(input).not.toHaveProperty('effort');
    expect(input).not.toHaveProperty('maxFires');
    // A3/A4: the working directory and the unattended flag no longer exist.
    expect(input).not.toHaveProperty('workdir');
    expect(input).not.toHaveProperty('unattended');
    expect(input).not.toHaveProperty('permissionPosture');
    expect(input.permissionMode).toBe('manual');
  });

  it('Given an interval schedule with an end date, When folded, Then the interval spec rides and only the cron key stays absent', () => {
    const input = cronDraftToInput({
      ...EMPTY_CRON_DRAFT,
      schedule: {
        kind: 'custom',
        mode: 'interval',
        interval: { every: 2, unit: 'day', time: '09:00' },
        ends: { kind: 'on', date: '2026-11-20' },
      },
      prompt: 'p',
      timezone: 'UTC',
      maxFiresText: '5',
    });
    // SDR-113: the compiled spec's anchorDate is the SUBMIT day (the
    // caller may pin it); the interval's own anchor is a schedule shape
    // detail the wire never carries on creation.
    expect(input.interval).toMatchObject({ every: 2, unit: 'day', time: '09:00' });
    expect(input.interval?.anchorDate).toMatch(/^\d{4}-\d{2}-\d{2}$/);
    expect(input).not.toHaveProperty('cron');
    // The budget belongs to RECURRING tasks; an ends date makes this rule the
    // one-off candidate, so no cap rides (the engine never budgets a one-off).
    expect(input.maxFires).toBeUndefined();
    expect(input.endsAt).toBeTruthy();
  });

  it('Given a draft with no schedule, When folded, Then the fold refuses', () => {
    expect(() => cronDraftToInput(EMPTY_CRON_DRAFT)).toThrow();
  });
});

describe('cronDraftRecurring', () => {
  it('Given the six presets, When derived, Then they are all recurring', () => {
    expect(cronDraftRecurring({ kind: 'hourly', minute: 0 })).toBe(true);
    expect(cronDraftRecurring({ kind: 'daily', time: '09:00' })).toBe(true);
    expect(cronDraftRecurring({ kind: 'weekday', time: '09:00' })).toBe(true);
    expect(cronDraftRecurring({ kind: 'weekly', days: [1], time: '09:00' })).toBe(true);
    expect(cronDraftRecurring({ kind: 'monthly', day: 1, time: '09:00' })).toBe(true);
    expect(
      cronDraftRecurring({ kind: 'custom', mode: 'raw', raw: { expr: '0 9 * * *' }, ends: { kind: 'never' } }),
    ).toBe(true);
  });

  it('Given an ends date equal to the t0 day in the task zone, When derived, Then the rule is the one-off', () => {
    // 2026-11-02T00:05:00Z is 2026-11-02 09:05 in Asia/Tokyo.
    const t0 = Date.parse('2026-11-02T00:05:00Z');
    const schedule = {
      kind: 'custom' as const,
      mode: 'raw' as const,
      raw: { expr: '5 9 2 11 *' },
      ends: { kind: 'on' as const, date: '2026-11-02' },
    };
    expect(cronDraftRecurring(schedule, t0, 'Asia/Tokyo')).toBe(false);
  });

  it('Given an ends date after the t0 day, When derived, Then the rule stays recurring', () => {
    const t0 = Date.parse('2026-11-02T00:05:00Z');
    const schedule = {
      kind: 'custom' as const,
      mode: 'raw' as const,
      raw: { expr: '5 9 * * *' },
      ends: { kind: 'on' as const, date: '2026-11-20' },
    };
    expect(cronDraftRecurring(schedule, t0, 'Asia/Tokyo')).toBe(true);
  });

  it('Given an ends date and no preview yet, When derived, Then the candidate reads not-recurring (unconfirmed)', () => {
    const schedule = {
      kind: 'custom' as const,
      mode: 'raw' as const,
      raw: { expr: '0 9 * * *' },
      ends: { kind: 'on' as const, date: '2026-11-20' },
    };
    expect(cronDraftRecurring(schedule, null, 'Asia/Tokyo')).toBe(false);
  });
});

describe('zonedDateOf / localDateOf', () => {
  it('Given an instant and a zone, When read, Then it yields the zone-local date', () => {
    expect(zonedDateOf(Date.parse('2026-11-01T20:00:00Z'), 'Asia/Tokyo')).toBe('2026-11-02');
    expect(zonedDateOf(Date.parse('2026-11-01T20:00:00Z'), 'UTC')).toBe('2026-11-01');
  });

  it('Given an unknown zone, When read, Then it degrades to the local date', () => {
    const at = Date.parse('2026-11-01T12:00:00Z');
    expect(zonedDateOf(at, 'Not/AZone')).toBe(localDateOf(at));
  });
});

// --- the prediction timeline (A5 / B1 / B2) --------------------------------

describe('predictionTimeline', () => {
  it('Given a preview with a past and two future moments, When built, Then the timeline reads past → next → next-next', () => {
    const timeline = predictionTimeline(
      { previousT0: 1000, occurrences: [2000, 3000] },
      { timezone: 'Asia/Tokyo' },
    );
    expect(timeline.points).toEqual([
      { kind: 'past', at: 1000 },
      { kind: 'next', at: 2000 },
      { kind: 'nextNext', at: 3000 },
    ]);
    expect(timeline.timezone).toBe('Asia/Tokyo');
  });

  it('Given a one-off rule (no previousT0, a single occurrence), When built, Then the past and the second future read null (the UI dash)', () => {
    const timeline = predictionTimeline({ previousT0: null, occurrences: [2000] });
    expect(timeline.points).toEqual([
      { kind: 'past', at: null },
      { kind: 'next', at: 2000 },
      { kind: 'nextNext', at: null },
    ]);
  });

  it('Given a high-frequency rule, When the caller drops the past, Then only the two future points stay', () => {
    const timeline = predictionTimeline(
      { previousT0: 1000, occurrences: [2000, 3000] },
      { showPast: false },
    );
    expect(timeline.points.map((point) => point.kind)).toEqual(['next', 'nextNext']);
  });

  it('Given no preview at all, When built, Then every point degrades to null honestly', () => {
    expect(predictionTimeline(null).points).toEqual([
      { kind: 'past', at: null },
      { kind: 'next', at: null },
      { kind: 'nextNext', at: null },
    ]);
  });
});

describe('rulePastInstant', () => {
  it('Given the engine answered previousT0, When read, Then it is the theoretical past', () => {
    expect(rulePastInstant({ previousT0: 42 })).toBe(42);
  });

  it('Given the engine answered null, When read, Then the past reads null (never invented)', () => {
    expect(rulePastInstant({ previousT0: null })).toBeNull();
    expect(rulePastInstant(undefined)).toBeNull();
  });
});

describe('isHighFrequencySchedule', () => {
  it('Given an hourly schedule, When checked, Then it is high-frequency', () => {
    expect(isHighFrequencySchedule('hourly')).toBe(true);
  });

  it('Given a daily schedule or no schedule, When checked, Then it is not', () => {
    expect(isHighFrequencySchedule('daily')).toBe(false);
    expect(isHighFrequencySchedule(undefined)).toBe(false);
  });
});

describe('zoneOffsetLabel (B2)', () => {
  it('Given a zone, When labelled, Then it renders the short offset of the zone, never the host zone', () => {
    expect(zoneOffsetLabel(Date.parse('2026-11-01T00:00:00Z'), 'Asia/Tokyo')).toBe('GMT+9');
    expect(zoneOffsetLabel(Date.parse('2026-11-01T00:00:00Z'), 'UTC')).toBe('GMT');
    // A zone whose offset differs from the host's proves the label follows
    // the ZONE, not the machine. 2026-11-01 is still EDT in New York, so the
    // DST season (not just the base offset) reaches the label.
    expect(zoneOffsetLabel(Date.parse('2026-11-01T00:00:00Z'), 'America/New_York')).toBe('GMT-4');
    expect(zoneOffsetLabel(Date.parse('2026-12-01T00:00:00Z'), 'America/New_York')).toBe('GMT-5');
    expect(zoneOffsetLabel(Date.parse('2026-11-01T00:00:00Z'), 'Asia/Kolkata')).toBe('GMT+5:30');
  });

  it('Given an empty or local zone, When labelled, Then it says local', () => {
    expect(zoneOffsetLabel(0, '')).toBe('local');
    expect(zoneOffsetLabel(0, 'local')).toBe('local');
  });

  it('Given an unknown zone, When labelled, Then the zone name passes through (honest degrade)', () => {
    expect(zoneOffsetLabel(0, 'Not/AZone')).toBe('Not/AZone');
  });
});

// --- run facts -------------------------------------------------------------

describe('lastRealRunAt', () => {
  it('Given runs including a skipped detection, When read, Then the latest non-skipped start is the fact (A5)', () => {
    expect(
      lastRealRunAt([
        { startedAt: '2026-11-01T00:00:00Z', status: 'succeeded' },
        { startedAt: '2026-11-02T00:00:00Z', status: 'skipped' },
      ]),
    ).toBe(Date.parse('2026-11-01T00:00:00Z'));
  });

  it('Given no real run, When read, Then it is null (never the prediction)', () => {
    expect(lastRealRunAt([])).toBeNull();
    expect(lastRealRunAt([{ startedAt: 'bad', status: 'succeeded' }])).toBeNull();
  });
});

describe('draftFromTask (A9 edit prefill)', () => {
  it('Given a task created without an explicit zone, When prefilled, Then the caller fallback zone rides (never an empty picker)', () => {
    expect(draftFromTask(task({ timezone: null }), 'Asia/Tokyo').timezone).toBe('Asia/Tokyo');
  });

  it('Given a task with its own zone, When prefilled, Then the task zone wins over the fallback', () => {
    expect(draftFromTask(task({ timezone: 'Europe/Berlin' }), 'Asia/Tokyo').timezone).toBe(
      'Europe/Berlin',
    );
  });
});
