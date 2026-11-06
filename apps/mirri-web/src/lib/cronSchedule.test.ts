// apps/mirri-web/src/lib/cronSchedule.test.ts
// The structured schedule's single-source guarantee (form batch 7 /
// ADR-016 §6, R4 显示=实际): every preset compiles to the exact
// expression its description states — table-driven (schedule, expected
// expression, expected description) triples; the interval shape stays
// the structured spec (never a pseudo-expression); the ends boundary
// rides both shapes isomorphically; raw passes through honestly.

import { describe, expect, it } from 'vitest';

import {
  compileSchedule,
  describeSchedule,
  isOneOffCandidate,
  newSchedule,
  scheduleComplete,
  todayLocalDate,
  type CronSchedule,
} from './cronSchedule';

/** A translator that renders the i18n templates the same shape the real
 * locale files carry — the assertion checks the PARAMS ride verbatim,
 * not the translated prose. */
const t = (key: string, params?: Record<string, unknown>): string =>
  params === undefined ? `{${key}}` : `{${key}:${JSON.stringify(params)}}`;

describe('compileSchedule (the six presets — ADR-016 §1.3)', () => {
  it.each([
    [{ kind: 'hourly', minute: 3 } as CronSchedule, '3 * * * *'],
    [{ kind: 'hourly', minute: 0 } as CronSchedule, '0 * * * *'],
    [{ kind: 'daily', time: '09:03' } as CronSchedule, '3 9 * * *'],
    [{ kind: 'weekday', time: '09:03' } as CronSchedule, '3 9 * * 1-5'],
    [{ kind: 'weekly', days: [1, 5], time: '09:03' } as CronSchedule, '3 9 * * 1,5'],
    // Multi-select order is normalized — the picker never depends on
    // the user's click order.
    [{ kind: 'weekly', days: [5, 1], time: '09:03' } as CronSchedule, '3 9 * * 1,5'],
    [{ kind: 'monthly', day: 1, time: '09:03' } as CronSchedule, '3 9 1 * *'],
    [
      {
        kind: 'custom',
        mode: 'raw',
        raw: { expr: ' */5 * * * * ' },
        ends: { kind: 'never' },
      } as CronSchedule,
      '*/5 * * * *',
    ],
  ])('Given the schedule %j, When compiled, Then it produces the expression %s', (schedule, expr) => {
    const compiled = compileSchedule(schedule, 'Asia/Tokyo');
    expect(compiled).toEqual({ type: 'cron', cron: expr, endBoundary: null });
  });

  it('Given a custom field schedule, When compiled, Then the picks ride the matching expression field', () => {
    const compiled = compileSchedule(
      {
        kind: 'custom',
        mode: 'field',
        field: { unit: 'day-of-week', picks: [3, 1], time: '09:03' },
        ends: { kind: 'never' },
      },
      'UTC',
    );
    expect(compiled).toEqual({ type: 'cron', cron: '3 9 * * 1,3', endBoundary: null });

    const byMonth = compileSchedule(
      {
        kind: 'custom',
        mode: 'field',
        field: { unit: 'day-of-month', picks: [15, 1], time: '09:03' },
        ends: { kind: 'never' },
      },
      'UTC',
    );
    expect(byMonth).toEqual({ type: 'cron', cron: '3 9 1,15 * *', endBoundary: null });
  });
});

describe('compileSchedule (the interval shape — SDR-113)', () => {
  it('Given a custom interval schedule, When compiled, Then it produces the structured IntervalSpec (never a pseudo-expression)', () => {
    const compiled = compileSchedule(
      {
        kind: 'custom',
        mode: 'interval',
        interval: { every: 2, unit: 'day', time: '09:30' },
        ends: { kind: 'never' },
      },
      'Asia/Tokyo',
      '2026-11-01',
    );
    expect(compiled).toEqual({
      type: 'interval',
      interval: { every: 2, unit: 'day', time: '09:30', anchorDate: '2026-11-01' },
      endBoundary: null,
    });
  });

  it('Given no explicit anchor, When an interval compiles, Then the anchor defaults to today (the submit-day — the wire has no anchor default)', () => {
    const compiled = compileSchedule({
      kind: 'custom',
      mode: 'interval',
      interval: { every: 1, unit: 'week', time: '09:00' },
      ends: { kind: 'never' },
    });
    expect(compiled.type).toBe('interval');
    if (compiled.type === 'interval') {
      expect(compiled.interval.anchorDate).toBe(todayLocalDate());
    }
  });
});

describe('compileSchedule (the ends boundary — SDR-110, both shapes isomorphic)', () => {
  it('Given a preset with no ends, When compiled, Then endBoundary stays null (six presets carry no ends of their own)', () => {
    const compiled = compileSchedule({ kind: 'daily', time: '09:00' }, 'UTC');
    expect(compiled.type).toBe('cron');
    if (compiled.type === 'cron') expect(compiled.endBoundary).toBeNull();
  });

  it('Given a custom schedule with ends.on, When compiled, Then the date rides endBoundary for both shapes', () => {
    const cronShape = compileSchedule(
      {
        kind: 'custom',
        mode: 'raw',
        raw: { expr: '3 9 * * *' },
        ends: { kind: 'on', date: '2026-12-31' },
      },
      'UTC',
    );
    expect(cronShape.type).toBe('cron');
    if (cronShape.type === 'cron') expect(cronShape.endBoundary).toBe('2026-12-31');

    const intervalShape = compileSchedule(
      {
        kind: 'custom',
        mode: 'interval',
        interval: { every: 2, unit: 'day', time: '09:30' },
        ends: { kind: 'on', date: '2026-12-31' },
      },
      'UTC',
      '2026-11-01',
    );
    expect(intervalShape.type).toBe('interval');
    if (intervalShape.type === 'interval') {
      expect(intervalShape.endBoundary).toBe('2026-12-31');
    }
  });
});

describe('describeSchedule (R4: the description states what the schedule does)', () => {
  it.each([
    [{ kind: 'hourly', minute: 3 } as CronSchedule, 'settings.cronRuleDescHourly', { minute: ':03' }],
    [{ kind: 'daily', time: '09:03' } as CronSchedule, 'settings.cronRuleDescDaily', { time: '09:03' }],
    [{ kind: 'weekday', time: '09:03' } as CronSchedule, 'settings.cronRuleDescWeekday', { time: '09:03' }],
    [
      { kind: 'weekly', days: [5, 1], time: '09:03' } as CronSchedule,
      'settings.cronRuleDescWeekly',
      {
        days: '{settings.cronRuleWeekdayShort_1}, {settings.cronRuleWeekdayShort_5}',
        time: '09:03',
      },
    ],
    [{ kind: 'monthly', day: 1, time: '09:03' } as CronSchedule, 'settings.cronRuleDescMonthly', { day: 1, time: '09:03' }],
  ])('Given the schedule %j, When described, Then the key %s carries the params verbatim', (schedule, key, params) => {
    expect(describeSchedule(schedule, 'Asia/Tokyo', t)).toBe(
      `Asia/Tokyo {${key}:${JSON.stringify(params)}}`,
    );
  });

  it('Given an interval schedule, When described, Then the elapsed-interval phrasing carries every/unit/time', () => {
    expect(
      describeSchedule(
        {
          kind: 'custom',
          mode: 'interval',
          interval: { every: 2, unit: 'day', time: '09:30' },
          ends: { kind: 'never' },
        },
        'Asia/Tokyo',
        t,
      ),
    ).toBe(
      'Asia/Tokyo {settings.cronRuleDescInterval:{"every":2,"unit":"{settings.cronRuleIntervalUnit_day}","time":"09:30"}}',
    );
  });

  it('Given an interval rule with every=1 day, When described, Then it reads 每天 not 每 1 天', () => {
    expect(
      describeSchedule(
        {
          kind: 'custom',
          mode: 'interval',
          interval: { every: 1, unit: 'day', time: '09:30' },
          ends: { kind: 'never' },
        },
        'UTC',
        t,
      ),
    ).toBe('UTC {settings.cronRuleDescIntervalEveryOne:{"time":"09:30"}}');
  });

  it('Given an interval rule with every=1 week, When described, Then it reads 每周 not 每 1 周', () => {
    expect(
      describeSchedule(
        {
          kind: 'custom',
          mode: 'interval',
          interval: { every: 1, unit: 'week', time: '09:30' },
          ends: { kind: 'never' },
        },
        'UTC',
        t,
      ),
    ).toBe('UTC {settings.cronRuleDescIntervalEveryOneWeek:{"time":"09:30"}}');
  });

  it('Given a raw schedule, When described, Then the honest fallback shows the expression itself', () => {
    expect(
      describeSchedule(
        { kind: 'custom', mode: 'raw', raw: { expr: '*/5 * * * *' }, ends: { kind: 'never' } },
        'UTC',
        t,
      ),
    ).toBe('UTC {settings.cronRuleDescRaw:{"expr":"*/5 * * * *"}}');
  });

  it('Given a weekly schedule with no days selected, When described, Then the incomplete phrasing names the missing choice (never a dangling "Weekly on at 09:03")', () => {
    expect(describeSchedule({ kind: 'weekly', days: [], time: '09:03' }, 'UTC', t)).toBe(
      'UTC {settings.cronRuleDescWeeklyNoDays}',
    );
  });

  it('Given a custom schedule with ends.on, When described, Then the inclusive-end suffix rides regardless of the first-fire day (the one-off phrasing is the caller\'s composition on the preview t0)', () => {
    expect(
      describeSchedule(
        {
          kind: 'custom',
          mode: 'interval',
          interval: { every: 1, unit: 'day', time: '09:00' },
          ends: { kind: 'on', date: '2026-12-31' },
        },
        'UTC',
        t,
      ),
    ).toContain('{settings.cronRuleDescEndsOn:{"date":"2026-12-31"}}');
    // The presets carry no ends of their own — no suffix there.
    expect(describeSchedule({ kind: 'daily', time: '09:00' }, 'UTC', t)).not.toContain('cronRuleDescEndsOn');
  });

  it('Given an empty timezone, When described, Then the prefix falls back to UTC (never an ambiguous blank)', () => {
    expect(describeSchedule({ kind: 'daily', time: '09:00' }, '', t)).toMatch(/^UTC /);
    expect(describeSchedule({ kind: 'daily', time: '09:00' }, '  ', t)).toMatch(/^UTC /);
  });
});

describe('isOneOffCandidate (SDR-110: one-off is derived, never a switch)', () => {
  it('Given each schedule shape, When checked, Then only custom+ends.on is the one-off candidate (the t0 comparison itself is the form layer\'s, against the backend preview)', () => {
    expect(isOneOffCandidate({ kind: 'daily', time: '09:00' })).toBe(false);
    expect(
      isOneOffCandidate({
        kind: 'custom',
        mode: 'interval',
        interval: { every: 1, unit: 'day', time: '09:00' },
        ends: { kind: 'never' },
      }),
    ).toBe(false);
    expect(
      isOneOffCandidate({
        kind: 'custom',
        mode: 'interval',
        interval: { every: 1, unit: 'day', time: '09:00' },
        ends: { kind: 'on', date: '2026-11-05' },
      }),
    ).toBe(true);
  });

  // --- form batch 8 (VU12): the one-off DECISION rides the preview t0 ---
  it('Given the candidate and the backend preview t0, When the t0 day (in the task timezone) equals the ends day, Then the schedule is the one-off (the「将于 X 日执行一次后结束」branch)', async () => {
    const { cronDraftRecurring, zonedDateOf } = await import('./cronManagement');
    const candidate: CronSchedule = {
      kind: 'custom',
      mode: 'interval',
      interval: { every: 1, unit: 'day', time: '09:00' },
      ends: { kind: 'on', date: '2026-11-05' },
    };
    const t0 = new Date(2026, 10, 5, 9).getTime();
    expect(isOneOffCandidate(candidate)).toBe(true);
    expect(zonedDateOf(t0, 'Asia/Shanghai')).toBe('2026-11-05');
    expect(cronDraftRecurring(candidate, t0, 'Asia/Shanghai')).toBe(false);
  });
});

describe('newSchedule (the structural preset reset — 切换不保留)', () => {
  it('Given a preset pick, When a new schedule is minted, Then each kind starts at its own neutral defaults (no sub-option leaks across kinds)', () => {
    expect(newSchedule('hourly')).toEqual({ kind: 'hourly', minute: 0 });
    expect(newSchedule('daily')).toEqual({ kind: 'daily', time: '09:00' });
    expect(newSchedule('weekday')).toEqual({ kind: 'weekday', time: '09:00' });
    expect(newSchedule('weekly')).toEqual({ kind: 'weekly', days: [1], time: '09:00' });
    expect(newSchedule('monthly')).toEqual({ kind: 'monthly', day: 1, time: '09:00' });
    expect(newSchedule('custom')).toEqual({
      kind: 'custom',
      mode: 'interval',
      interval: { every: 1, unit: 'day', time: '09:00' },
      ends: { kind: 'never' },
    });
  });

  it('Given a previously-configured weekly schedule, When the preset switches to monthly, Then the minted value carries no weekday residue', () => {
    const weekly: CronSchedule = { kind: 'weekly', days: [1, 2, 3, 4, 5], time: '18:45' };
    const switched = newSchedule('monthly');
    expect(switched).toEqual({ kind: 'monthly', day: 1, time: '09:00' });
    expect((switched as typeof weekly & { days?: number[] }).days).toBeUndefined();
  });
});

describe('scheduleComplete (the honest-minimum submittable gate)', () => {
  it('Given each complete preset, When checked, Then it is complete', () => {
    expect(scheduleComplete({ kind: 'hourly', minute: 3 })).toBe(true);
    expect(scheduleComplete({ kind: 'daily', time: '09:03' })).toBe(true);
    expect(scheduleComplete({ kind: 'weekday', time: '09:03' })).toBe(true);
    expect(scheduleComplete({ kind: 'weekly', days: [1], time: '09:03' })).toBe(true);
    expect(scheduleComplete({ kind: 'monthly', day: 1, time: '09:03' })).toBe(true);
  });

  it('Given shapes with missing or out-of-range fields, When checked, Then they stay incomplete', () => {
    expect(scheduleComplete({ kind: 'hourly', minute: 60 })).toBe(false);
    expect(scheduleComplete({ kind: 'daily', time: '9:3' })).toBe(false);
    expect(scheduleComplete({ kind: 'weekly', days: [], time: '09:03' })).toBe(false);
    expect(scheduleComplete({ kind: 'monthly', day: 32, time: '09:03' })).toBe(false);
  });

  it('Given custom shapes, When checked, Then completeness follows the mode (interval spec / field picks / non-empty raw)', () => {
    expect(
      scheduleComplete({
        kind: 'custom',
        mode: 'interval',
        interval: { every: 2, unit: 'day', time: '09:30' },
        ends: { kind: 'never' },
      }),
    ).toBe(true);
    expect(
      scheduleComplete({
        kind: 'custom',
        mode: 'interval',
        interval: { every: 0, unit: 'day', time: '09:30' },
        ends: { kind: 'never' },
      }),
    ).toBe(false);
    expect(
      scheduleComplete({
        kind: 'custom',
        mode: 'raw',
        raw: { expr: '   ' },
        ends: { kind: 'never' },
      }),
    ).toBe(false);
    expect(
      scheduleComplete({
        kind: 'custom',
        mode: 'field',
        field: { unit: 'day-of-week', picks: [1, 8], time: '09:00' },
        ends: { kind: 'never' },
      }),
    ).toBe(false);
  });
});
