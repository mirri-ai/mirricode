// apps/mirri-web/src/lib/cronSchedule.ts
// The creation form's structured schedule (form batch 7 / ADR-016): the
// form state never holds a cron string — it holds the `CronSchedule`
// discriminated union, and this module is the single source that
// compiles it (to a cron expression OR an interval spec — SDR-113) and
// describes it (the readable preview, R4 显示=实际). Structure,
// compilation, and description close the loop in ONE testable module
// with no Vue dependency.
//
// Division of labor (ADR-016 §1.3/§2.4): the FIRST fire moment is never
// computed here — the backend `cron.preview` RPC owns occurrence
// evaluation (ADR-017). This module provides the one-off CANDIDATE
// shape (`isOneOffCandidate`); comparing ends.date against the
// preview's t0 (and the recurring derivation from it) is the form
// layer's job (batch 8), which owns the t0 truth.
//
// Round-trip symmetry (the 保存后回显 defect): `compileSchedule` and
// `scheduleFromExpression` are the two directions of ONE mapping, so a
// stored rule reopens in the shape the user entered it in — a preset stays a
// preset, a field rule stays a field rule, and only a genuinely
// un-representable expression falls back to the advanced (raw) form.

import type { AppCronInterval } from '../api/types';
import { humanizeCron } from './cronHumanize';

/** A wall-clock time `HH:MM` (0-23 : 0-59, zero-padded). */
export type ClockTime = string;

/** A calendar date `YYYY-MM-DD`. */
export type CalendarDate = string;

/** The six presets' kind ids (the closed enum — no extension point). */
export type ScheduleKind =
  | 'hourly'
  | 'daily'
  | 'weekday'
  | 'weekly'
  | 'monthly'
  | 'custom';

/** The inclusive end of a schedule (SDR-110): `on(date)` means the last
 * run never falls after that day — the day itself still fires. A
 * one-off is a derived property (first fire day === ends day), never a
 * separate switch. */
export type ScheduleEnds =
  | { kind: 'never' }
  | { kind: 'on'; date: CalendarDate };

/** The form's schedule — a discriminated union (ADR-016 §1.2). Switching
 * presets replaces the whole value (sub-options never leak across
 * kinds — the type system guarantees "切换不保留", not a UI
 * convention). JSON-serializable pure data. */
export type CronSchedule =
  | { kind: 'hourly'; minute: number }
  | { kind: 'daily'; time: ClockTime }
  | { kind: 'weekday'; time: ClockTime }
  | { kind: 'weekly'; days: number[]; time: ClockTime } // days: 1-7 (Mon=1), non-empty
  | { kind: 'monthly'; day: number; time: ClockTime } // day: 1-31
  | {
      kind: 'custom';
      mode: 'field' | 'interval' | 'raw';
      /** field mode — the cron-expressible combinations. */
      field?: {
        unit: 'day-of-month' | 'day-of-week' | 'hour-of-day' | 'minute-of-hour';
        picks: number[];
        time: ClockTime;
      };
      /** interval mode — the elapsed-interval semantic (SDR-113). The
       * anchor is the submit-day; the backend `IntervalSpec` requires
       * an explicit anchorDate (no wire default exists). */
      interval?: { every: number; unit: 'day' | 'week' | 'month'; time: ClockTime };
      /** raw mode — the advanced expression (field mode only, R5). */
      raw?: { expr: string };
      ends: ScheduleEnds;
    };

/** The custom-rule branch of the union (the dialog's scratch editor value). */
export type CustomSchedule = Extract<CronSchedule, { kind: 'custom' }>;

/** The compiled submission value (ADR-016 §1.3): a cron preset
 * translates into an expression; a custom interval stays the structured
 * spec (never a pseudo-expression). `endBoundary` carries the ends date
 * isomorphically for both types (the wire's `endsAt`). */
export type CompiledSchedule =
  | { type: 'cron'; cron: string; endBoundary: CalendarDate | null }
  | {
      type: 'interval';
      interval: AppCronInterval;
      endBoundary: CalendarDate | null;
    };

type Translator = (key: string, params?: Record<string, unknown>) => string;

/** The field-mode unit the picker's dialog offers, with its inclusive pick
 *  bounds (minute 0 / hour 0 are VALID picks). */
type ScheduleFieldUnit = NonNullable<CustomSchedule['field']>['unit'];

const PICK_BOUNDS: Record<ScheduleFieldUnit, { min: number; max: number }> = {
  'minute-of-hour': { min: 0, max: 59 },
  'hour-of-day': { min: 0, max: 23 },
  'day-of-month': { min: 1, max: 31 },
  'day-of-week': { min: 1, max: 7 },
};

/** The neutral wall-clock time a field rule carries when its unit has no time
 *  of its own (a minute-of-hour / hour-of-day rule). Keeping an explicit value
 *  means switching the unit inside the dialog stays coherent. */
const DEFAULT_FIELD_TIME: ClockTime = '09:00';

const DAY_KEYS: Record<number, string> = {
  1: 'settings.cronRuleWeekdayShort_1',
  2: 'settings.cronRuleWeekdayShort_2',
  3: 'settings.cronRuleWeekdayShort_3',
  4: 'settings.cronRuleWeekdayShort_4',
  5: 'settings.cronRuleWeekdayShort_5',
  6: 'settings.cronRuleWeekdayShort_6',
  7: 'settings.cronRuleWeekdayShort_7',
};

/** The localized weekday label — the same `cronWeekdayShort_{d}` keys the
 * weekly multi-select renders with, so zh never shows "每周 Mon, Fri". */
function weekdayLabel(day: number, t: Translator): string {
  const key = DAY_KEYS[day];
  return key === undefined ? String(day) : t(key);
}

function isValidClockTime(time: string): boolean {
  const match = /^(\d{2}):(\d{2})$/.exec(time);
  if (!match) return false;
  const hour = Number(match[1]);
  const minute = Number(match[2]);
  return hour <= 23 && minute <= 59;
}

/** The ends boundary the compiled schedule carries: `null` for never,
 * the date verbatim for `on` (the inclusive last-fire day). */
export function endsBoundary(ends: ScheduleEnds): CalendarDate | null {
  return ends.kind === 'on' ? ends.date : null;
}

/** Whether the schedule is structurally complete enough to compile and
 * submit (the honest-minimum submittable convention: field-shaped
 * schedules are valid when their fields are; the server stays the
 * authoritative validator — the preview RPC is a display projection). */
export function scheduleComplete(schedule: CronSchedule): boolean {
  switch (schedule.kind) {
    case 'hourly':
      return Number.isInteger(schedule.minute) && schedule.minute >= 0 && schedule.minute <= 59;
    case 'daily':
    case 'weekday':
      return isValidClockTime(schedule.time);
    case 'weekly':
      return (
        isValidClockTime(schedule.time) &&
        schedule.days.length > 0 &&
        schedule.days.every((d) => d >= 1 && d <= 7)
      );
    case 'monthly':
      return (
        isValidClockTime(schedule.time) &&
        Number.isInteger(schedule.day) &&
        schedule.day >= 1 &&
        schedule.day <= 31
      );
    case 'custom': {
      if (schedule.mode === 'interval') {
        const interval = schedule.interval;
        return (
          interval !== undefined &&
          Number.isInteger(interval.every) &&
          interval.every >= 1 &&
          (interval.unit === 'day' || interval.unit === 'week' || interval.unit === 'month') &&
          isValidClockTime(interval.time)
        );
      }
      if (schedule.mode === 'raw') {
        return (schedule.raw?.expr.trim() ?? '') !== '';
      }
      const field = schedule.field;
      if (field === undefined) return false;
      const validPicks = (() => {
        switch (field.unit) {
          case 'minute-of-hour':
            return field.picks.every((p) => p >= 0 && p <= 59);
          case 'hour-of-day':
            return field.picks.every((p) => p >= 0 && p <= 23);
          case 'day-of-month':
            return field.picks.every((p) => p >= 1 && p <= 31);
          case 'day-of-week':
            return field.picks.every((p) => p >= 1 && p <= 7) && isValidClockTime(field.time);
        }
      })();
      return validPicks && field.picks.length > 0;
    }
  }
}

function pad2(n: number): string {
  return String(n).padStart(2, '0');
}

/** Today's local date `YYYY-MM-DD` — the interval spec's default anchor
 * (the backend `IntervalSpec` requires an explicit anchorDate; the
 * first fire is the first `time` strictly after the anchor moment). */
export function todayLocalDate(now: Date = new Date()): CalendarDate {
  const year = now.getFullYear();
  const month = pad2(now.getMonth() + 1);
  const day = pad2(now.getDate());
  return `${year}-${month}-${day}`;
}

/** Whether the schedule's first fire may equal its end day — the one-off
 * CANDIDATE shape (SDR-110): a custom schedule with an ends date. The
 * actual t0 comparison happens in the form layer against the backend
 * preview (ADR-017 — the frontend never computes occurrences); this is
 * the pure-function boolean the form branches on. */
export function isOneOffCandidate(schedule: CronSchedule): boolean {
  return schedule.kind === 'custom' && schedule.ends.kind === 'on';
}

/** The ends date of a one-off CANDIDATE (callers gate on
 *  [`isOneOffCandidate`] first); '' when the Custom dialog has not
 *  picked a date yet. */
export function endsDateOf(schedule: CronSchedule): CalendarDate {
  return schedule.kind === 'custom' && schedule.ends.kind === 'on'
    ? schedule.ends.date
    : '';
}

/** Compile the structured schedule into its submission value
 * (ADR-016 §1.3). Cron presets are direct translations (hourly at :03 →
 * `3 * * * *`, daily 09:03 → `3 9 * * *`, weekday → `3 9 * * 1-5`,
 * weekly multi-select → the dow list, monthly → dom, custom field/raw →
 * the synthesized expression); a custom interval stays the structured
 * spec — never a pseudo-expression. `anchorDate` defaults to today
 * (the submit-day; the caller may pin it for tests). */
export function compileSchedule(
  schedule: CronSchedule,
  timezone?: string,
  anchorDate: CalendarDate = todayLocalDate(),
): CompiledSchedule {
  void timezone; // the timezone is a display/evaluation concern — the compiled shape never varies by zone
  const ends = schedule.kind === 'custom' ? endsBoundary(schedule.ends) : null;
  switch (schedule.kind) {
    case 'hourly':
      return { type: 'cron', cron: `${schedule.minute} * * * *`, endBoundary: ends };
    case 'daily': {
      const [hour, minute] = schedule.time.split(':').map(Number);
      return { type: 'cron', cron: `${minute} ${hour} * * *`, endBoundary: ends };
    }
    case 'weekday': {
      const [hour, minute] = schedule.time.split(':').map(Number);
      return { type: 'cron', cron: `${minute} ${hour} * * 1-5`, endBoundary: ends };
    }
    case 'weekly': {
      const [hour, minute] = schedule.time.split(':').map(Number);
      const dow = [...schedule.days].toSorted((a, b) => a - b).join(',');
      return { type: 'cron', cron: `${minute} ${hour} * * ${dow}`, endBoundary: ends };
    }
    case 'monthly': {
      const [hour, minute] = schedule.time.split(':').map(Number);
      return { type: 'cron', cron: `${minute} ${hour} ${schedule.day} * *`, endBoundary: ends };
    }
    case 'custom': {
      if (schedule.mode === 'interval') {
        const interval = schedule.interval;
        if (interval === undefined) {
          throw new Error('compileSchedule: a custom interval schedule carries no interval spec');
        }
        return {
          type: 'interval',
          interval: {
            every: interval.every,
            unit: interval.unit,
            time: interval.time,
            anchorDate,
          },
          endBoundary: ends,
        };
      }
      if (schedule.mode === 'raw') {
        return { type: 'cron', cron: schedule.raw?.expr.trim() ?? '', endBoundary: ends };
      }
      const field = schedule.field;
      if (field === undefined) {
        throw new Error('compileSchedule: a custom field schedule carries no field spec');
      }
      const picks = [...field.picks].toSorted((a, b) => a - b).join(',');
      switch (field.unit) {
        case 'minute-of-hour':
          return { type: 'cron', cron: `${picks} * * * *`, endBoundary: ends };
        case 'hour-of-day':
          return { type: 'cron', cron: `0 ${picks} * * *`, endBoundary: ends };
        case 'day-of-month': {
          const [hour, minute] = field.time.split(':').map(Number);
          return { type: 'cron', cron: `${minute} ${hour} ${picks} * *`, endBoundary: ends };
        }
        case 'day-of-week': {
          const [hour, minute] = field.time.split(':').map(Number);
          return { type: 'cron', cron: `${minute} ${hour} * * ${picks}`, endBoundary: ends };
        }
      }
    }
  }
}

/** The timezone prefix of the readable description — the user-facing
 * zone name (IANA or the raw value), consistent across the description
 * and the three-moment preview. */
export function scheduleTimezoneLabel(timezone: string): string {
  const trimmed = timezone.trim();
  return trimmed === '' ? 'UTC' : trimmed;
}

function isPlainNumber(field: string): boolean {
  return /^\d+$/.test(field);
}

/** The comma list of one field as numbers, or null when any pick is not an
 * integer inside `[min, max]` (an out-of-range pick must never be rounded
 * into a preset). */
function parsePickerList(field: string, min: number, max: number): number[] | null {
  if (!/^\d+(,\d+)*$/.test(field)) return null;
  const picks = [...new Set(field.split(',').map(Number))].toSorted((a, b) => a - b);
  return picks.every((pick) => pick >= min && pick <= max) ? picks : null;
}

function clockOf(hour: number, minute: number): ClockTime {
  return `${pad2(hour)}:${pad2(minute)}`;
}

/** The picker shape a stored cron expression stands for — the INVERSE of
 *  `compileSchedule` (R4 显示=实际, echo side). Everything `compileSchedule`
 *  can emit comes back as the shape that produced it: the six presets as
 *  presets, a field rule as its structured options. An expression with no
 *  structured equivalent (every-minute, step values, month filters, a
 *  day-of-week list that is byte-identical to the weekly preset) answers null,
 *  and the caller keeps it in the advanced (raw) form.
 *
 *  `ends` is always `never` here: the end date is the task's own field, not a
 *  property of the expression — `scheduleFromTask` folds it in. */
export function scheduleFromExpression(expr: string): CronSchedule | null {
  const fields = expr.trim().split(/\s+/);
  if (fields.length !== 5) return null;
  const [minute, hour, dom, month, dow] = fields as [string, string, string, string, string];
  // A month filter has no picker equivalent at all.
  if (month !== '*') return null;

  const ends: ScheduleEnds = { kind: 'never' };
  const fieldRule = (
    unit: ScheduleFieldUnit,
    picks: number[],
    time: ClockTime = DEFAULT_FIELD_TIME,
  ): CustomSchedule => ({
    kind: 'custom',
    mode: 'field',
    field: { unit, picks, time },
    ends,
  });

  if (!isPlainNumber(minute)) {
    // A minute pick list over every hour — 按字段重复 › 每小时几分.
    const picks = parsePickerList(minute, PICK_BOUNDS['minute-of-hour'].min, PICK_BOUNDS['minute-of-hour'].max);
    if (picks === null || picks.length < 2 || hour !== '*' || dom !== '*' || dow !== '*') return null;
    return fieldRule('minute-of-hour', picks);
  }
  const minuteNumber = Number(minute);
  if (minuteNumber > 59) return null;

  if (hour === '*') {
    // `M * * * *` — the 每小时 preset (the minute list case is handled above,
    // so a single minute here is unambiguous).
    return dom === '*' && dow === '*' ? { kind: 'hourly', minute: minuteNumber } : null;
  }

  if (!isPlainNumber(hour)) {
    // An hour pick list fires at :00 of each picked hour.
    const picks = parsePickerList(hour, PICK_BOUNDS['hour-of-day'].min, PICK_BOUNDS['hour-of-day'].max);
    if (picks === null || picks.length < 2 || minuteNumber !== 0 || dom !== '*' || dow !== '*') return null;
    return fieldRule('hour-of-day', picks);
  }
  const hourNumber = Number(hour);
  if (hourNumber > 23) return null;
  const time = clockOf(hourNumber, minuteNumber);

  if (dow === '*') {
    if (dom === '*') return { kind: 'daily', time };
    if (!isPlainNumber(dom)) {
      // A day-of-month pick list — 按字段重复 › 每月几号.
      const picks = parsePickerList(dom, PICK_BOUNDS['day-of-month'].min, PICK_BOUNDS['day-of-month'].max);
      if (picks === null || picks.length < 2) return null;
      return fieldRule('day-of-month', picks, time);
    }
    const day = Number(dom);
    if (day < 1 || day > 31) return null;
    return { kind: 'monthly', day, time };
  }

  if (dom !== '*') return null;
  if (dow === '1-5') return { kind: 'weekday', time };
  const days = parsePickerList(dow, PICK_BOUNDS['day-of-week'].min, PICK_BOUNDS['day-of-week'].max);
  // `parsePickerList` already range-checks; the weekly preset is the simpler
  // of the two equivalent shapes (a day-of-week list compiles identically).
  return days === null ? null : { kind: 'weekly', days, time };
}

/** A plain-data deep copy of a schedule value.
 *
 *  The pickers' models are Vue reactive (a deep Proxy), and a dialog writing
 *  through `v-model` can hand a NESTED proxy back into a ref — so the copy
 *  must be made without `structuredClone`, which throws `DataCloneError` on a
 *  Proxy (the reported 自定义规则确认后不生效). A `CronSchedule` is
 *  JSON-serializable pure data by contract, so one JSON round-trip is an exact
 *  copy: nested proxies are flattened, absent optionals stay absent, and the
 *  result is always cloneable again. */
export function cloneScheduleData<T extends CronSchedule>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

/** Fold the custom dialog's scratch state into the rule it confirms (A8).
 *
 *  Pure so the dialog has no logic of its own: the picks text is parsed into
 *  `field.picks` (valid numbers inside the unit's bounds, de-duplicated and
 *  sorted), and a non-empty Advanced expression wins over the structured
 *  options (confirming switches the mode to `raw`). The answer is plain data —
 *  the model must never hold a proxy. */
export function confirmCustomDraft(
  draft: CustomSchedule,
  picksText: string,
  rawText: string,
): CustomSchedule {
  const value = cloneScheduleData(draft);
  const expr = rawText.trim();
  // A non-empty expression wins over the structured options: in raw mode it
  // IS the rule (the mirror is the textarea's source of truth), and in field
  // mode it is the Advanced fold (confirm switches to raw).
  if (value.mode === 'raw') {
    value.raw = { expr };
  } else if (value.mode === 'field' && expr !== '') {
    value.mode = 'raw';
    value.raw = { expr };
  }
  if (value.mode === 'field') {
    const field = value.field ?? { unit: 'day-of-week' as const, picks: [1], time: DEFAULT_FIELD_TIME };
    const bounds = PICK_BOUNDS[field.unit];
    const picks = [
      ...new Set(
        picksText
          .split(',')
          .map((pick) => Number(pick.trim()))
          .filter((pick) => Number.isInteger(pick) && pick >= bounds.min && pick <= bounds.max),
      ),
    ].toSorted((a, b) => a - b);
    value.field = { ...field, picks };
  }
  if (value.mode === 'raw' && value.raw === undefined && expr !== '') {
    value.raw = { expr };
  }
  // A mode switch seeds every sub-option slot; an empty expression in a
  // structured mode carries no meaning, so it stays ABSENT (the wire's
  // absent-key convention) instead of riding an empty `raw` object.
  if (value.mode !== 'raw' && (value.raw?.expr.trim() ?? '') === '') {
    delete value.raw;
  }
  return value;
}

/** The readable RULE sentence (ADR-016 §1.4 / R4) — the words alone, without
 *  the zone prefix and without the end note. Every surface that shows a rule
 *  (the picker's live sentence, the task card's frequency line) reads THIS,
 *  so what a preview says while typing is exactly what the card says later.
 *
 *  A raw expression is humanized when the humanizer recognizes it
 *  (`* * * * *` → 每分钟) and shows the expression only when it cannot — the
 *  honest fallback, never a wrong friendly label. */
export function describeScheduleRule(schedule: CronSchedule, t: Translator): string {
  switch (schedule.kind) {
    case 'hourly':
      return t('settings.cronRuleDescHourly', { minute: `:${pad2(schedule.minute)}` });
    case 'daily':
      return t('settings.cronRuleDescDaily', { time: schedule.time });
    case 'weekday':
      return t('settings.cronRuleDescWeekday', { time: schedule.time });
    case 'weekly':
      return schedule.days.length === 0
        ? t('settings.cronRuleDescWeeklyNoDays')
        : t('settings.cronRuleDescWeekly', {
            days: [...schedule.days]
              .toSorted((a, b) => a - b)
              .map((d) => weekdayLabel(d, t))
              .join(', '),
            time: schedule.time,
          });
    case 'monthly':
      return t('settings.cronRuleDescMonthly', { day: schedule.day, time: schedule.time });
    case 'custom': {
      if (schedule.mode === 'interval') {
        const interval = schedule.interval;
        if (interval === undefined) return t('settings.cronRuleDescCustom');
        return interval.every === 1 && (interval.unit === 'day' || interval.unit === 'week')
          ? // SDR-116: every=1 reads the natural word (每天/每周), never 「每 1 天」.
            t(
              interval.unit === 'day'
                ? 'settings.cronRuleDescIntervalEveryOne'
                : 'settings.cronRuleDescIntervalEveryOneWeek',
              { time: interval.time },
            )
          : t('settings.cronRuleDescInterval', {
              every: interval.every,
              unit: t(`settings.cronRuleIntervalUnit_${interval.unit}`),
              time: interval.time,
            });
      }
      if (schedule.mode === 'raw') {
        const expr = schedule.raw?.expr ?? '';
        // R5's honest fallback — the same principle humanizeCron applies: an
        // expression the humanizer can read says so in words (每分钟), and one
        // it cannot shows the truth, never a wrong friendly label.
        const humanized = humanizeCron(expr, t);
        return humanized === expr
          ? t('settings.cronRuleDescRaw', { expr })
          : humanized;
      }
      const field = schedule.field;
      return field === undefined
        ? t('settings.cronRuleDescCustom')
        : describeFieldMode(field, t);
    }
  }
}

/** The inclusive-end note of a rule (`…（至 DATE 止，含当日）`), or '' when the
 *  rule has no end date. Presets carry no ends of their own — only the custom
 *  branch has the slot. */
export function describeScheduleEnds(schedule: CronSchedule, t: Translator): string {
  return schedule.kind === 'custom' && schedule.ends.kind === 'on'
    ? t('settings.cronRuleDescEndsOn', { date: schedule.ends.date })
    : '';
}

/** The readable description (ADR-016 §1.4 / R4): the rule sentence with the
 *  timezone as a visible prefix (VU8) and the inclusive end note. The one-off
 *  phrasing (ends day === first fire day) is the CALLER's composition on top
 *  of the preview t0 — this layer only states the inclusive end. */
export function describeSchedule(
  schedule: CronSchedule,
  timezone: string,
  t: Translator,
): string {
  const tz = scheduleTimezoneLabel(timezone);
  const ends = describeScheduleEnds(schedule, t);
  const endsSuffix = ends === '' ? '' : ` ${ends}`;
  return `${tz} ${describeScheduleRule(schedule, t)}${endsSuffix}`;
}

function describeFieldMode(
  field: NonNullable<Extract<CronSchedule, { kind: 'custom' }>['field']>,
  t: Translator,
): string {
  const picks = [...field.picks].toSorted((a, b) => a - b);
  switch (field.unit) {
    case 'minute-of-hour':
      return t('settings.cronRuleDescCustomMinutes', {
        minutes: picks.map((p) => `:${pad2(p)}`).join(', '),
      });
    case 'hour-of-day':
      return t('settings.cronRuleDescCustomHours', {
        hours: picks.map((p) => `${pad2(p)}:00`).join(', '),
      });
    case 'day-of-month':
      return t('settings.cronRuleDescCustomDaysOfMonth', { days: picks.join(', ') });
    case 'day-of-week':
      return t('settings.cronRuleDescCustomDaysOfWeek', {
        days: picks.map((d) => weekdayLabel(d, t)).join(', '),
        time: field.time,
      });
  }
}

/** A fresh schedule value per preset — the structural reset (切换档位
 * 整体重置子选项): each kind starts at its own neutral defaults, and
 * switching kinds replaces the whole object. */
export function newSchedule(kind: ScheduleKind): CronSchedule {
  switch (kind) {
    case 'hourly':
      return { kind: 'hourly', minute: 0 };
    case 'daily':
      return { kind: 'daily', time: '09:00' };
    case 'weekday':
      return { kind: 'weekday', time: '09:00' };
    case 'weekly':
      return { kind: 'weekly', days: [1], time: '09:00' };
    case 'monthly':
      return { kind: 'monthly', day: 1, time: '09:00' };
    case 'custom':
      return {
        kind: 'custom',
        mode: 'interval',
        interval: { every: 1, unit: 'day', time: '09:00' },
        ends: { kind: 'never' },
      };
  }
}
