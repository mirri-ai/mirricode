// apps/mirri-web/src/lib/cronManagement.ts
// Pure TS — no Vue. The scheduled-task page's view logic: the card list's
// status filter and ordering, the creation/edit form's draft → wire fold,
// the prediction timeline's model, and the small display helpers the cards
// and the detail pane share.
//
// Concept split (cron-improvement A7): the page manages the WORKSPACE's
// scheduled tasks only — session timers are the agent's tool behavior and
// are merely displayed in their own conversation (never managed here).

import type {
  AppCronPermissionMode,
  AppCronPreview,
  AppCronTask,
  AppCronTaskInput,
} from '../api/types';
import { compileSchedule, isOneOffCandidate, scheduleComplete, type CronSchedule } from './cronSchedule';

// ---------------------------------------------------------------------------
// The task list
// ---------------------------------------------------------------------------

/** The list page's status filter tabs (§5: status lives in the tab row, never
 *  as a per-card badge). */
export type ScheduledTaskFilter = 'all' | 'active' | 'paused' | 'failed';

/** The tag vocabulary a filter reads. */
export interface ScheduledTaskFilterCounts {
  all: number;
  active: number;
  paused: number;
  failed: number;
}

/** A failed last run (the honest status input the caller supplies — the
 *  list row itself carries no run verdict). */
export interface ScheduledTaskRunFacts {
  /** The task's most recent run failed / was rejected. */
  lastRunFailed: boolean;
}

/** The status bucket a task belongs to (A12: 启用状态 rides the filter
 *  instead of a card badge). */
export function scheduledTaskStatus(task: AppCronTask): 'active' | 'paused' | 'completed' {
  if (task.paused === true) return 'paused';
  // A fired one-shot, or a recurring task that spent its budget: nothing
  // more will fire — 已结束（按计划）.
  if (!task.recurring) return 'completed';
  // `maxFires` is null (wire) or undefined when the task is uncapped — only a
  // real number is a budget.
  if (typeof task.maxFires === 'number' && (task.fireCount ?? 0) >= task.maxFires) return 'completed';
  return 'active';
}

/** Whether a task passes the active filter tab. `failed` reads the caller's
 *  run facts. */
export function scheduledTaskMatchesFilter(
  task: AppCronTask,
  filter: ScheduledTaskFilter,
  facts?: ScheduledTaskRunFacts,
): boolean {
  const status = scheduledTaskStatus(task);
  switch (filter) {
    case 'all':
      return true;
    case 'active':
      return status === 'active';
    case 'paused':
      return status === 'paused';
    case 'failed':
      return facts?.lastRunFailed === true;
    default:
      return true;
  }
}

/** Filter then order: next-fire ascending (the soonest task first), tasks
 *  with no next fire sinking to the end, id breaking ties so the order is
 *  stable across refreshes. */
export function filterScheduledTasks(
  tasks: ReadonlyArray<AppCronTask>,
  filter: ScheduledTaskFilter,
  factsById: Readonly<Record<string, ScheduledTaskRunFacts>> = {},
): AppCronTask[] {
  return tasks
    .filter((task) => scheduledTaskMatchesFilter(task, filter, factsById[task.id]))
    .toSorted((a, b) => {
      const an = a.nextFireAt ?? Number.POSITIVE_INFINITY;
      const bn = b.nextFireAt ?? Number.POSITIVE_INFINITY;
      if (an !== bn) return an - bn;
      return a.id.localeCompare(b.id);
    });
}

/** The tab labels' counts. */
export function scheduledTaskFilterCounts(
  tasks: ReadonlyArray<AppCronTask>,
  factsById: Readonly<Record<string, ScheduledTaskRunFacts>> = {},
): ScheduledTaskFilterCounts {
  return {
    all: tasks.length,
    active: tasks.filter((task) => scheduledTaskMatchesFilter(task, 'active')).length,
    paused: tasks.filter((task) => scheduledTaskMatchesFilter(task, 'paused')).length,
    failed: tasks.filter((task) => scheduledTaskMatchesFilter(task, 'failed', factsById[task.id]))
      .length,
  };
}

/** The panel's grouped view of every workspace's tasks (A14): one group per
 *  workspace that HAS tasks, the active workspace's group first, then the
 *  rest by workspace name (the workspace id breaks name ties so the order is
 *  deterministic). A task without a workspace scope falls back to the ACTIVE
 *  workspace — the page addresses it there. A workspace with no tasks
 *  produces no group. */
export interface GroupedCronTasks {
  workspaceId: string;
  workspaceName: string;
  tasks: AppCronTask[];
}

export function groupTasksByWorkspace(
  tasks: ReadonlyArray<AppCronTask>,
  workspaceNames: ReadonlyArray<{ id: string; name: string }>,
  activeWorkspaceId: string,
): GroupedCronTasks[] {
  const byId = new Map(workspaceNames.map((w) => [w.id, w.name || w.id]));
  const groups = new Map<string, AppCronTask[]>();
  for (const task of tasks) {
    const ws = task.scope?.kind === 'workspace' ? task.scope.workspaceId : activeWorkspaceId;
    const list = groups.get(ws) ?? [];
    list.push(task);
    groups.set(ws, list);
  }
  return [...groups.entries()]
    .toSorted((a, b) => {
      if (a[0] === activeWorkspaceId) return -1;
      if (b[0] === activeWorkspaceId) return 1;
      const nameDiff = (byId.get(a[0]) ?? a[0]).localeCompare(byId.get(b[0]) ?? b[0]);
      return nameDiff !== 0 ? nameDiff : a[0].localeCompare(b[0]);
    })
    .map(([id, groupTasks]) => ({
      workspaceId: id,
      workspaceName: byId.get(id) ?? id,
      tasks: groupTasks,
    }));
}

/** The card's 已运行 N 次 label — the delivered fire count (E.3's n; the
 *  cap stays in the detail pane, the card stays quiet). */
export function scheduledTaskRunCount(task: { fireCount?: number }): number {
  return task.fireCount ?? 0;
}

// ---------------------------------------------------------------------------
// The creation / edit form's draft
// ---------------------------------------------------------------------------

/** The form's local state — the editable mirror of [`AppCronTaskInput`]
 *  whose numeric fields stay text until submit.
 *
 *  Concept split (A7/A3/A4): the form targets ONE workspace (the page's
 *  current workspace — no separate target arm, no working directory); the
 *  permission choice is the session's own three modes carried VERBATIM
 *  (no posture fold table); `unattended` is gone from the product面. */
export interface CronCreateDraft {
  schedule: CronSchedule | null;
  prompt: string;
  timezone: string;
  maxFiresText: string;
  /** The workspace the task belongs to (the page's current workspace; the
   *  edit form shows it read-only — A9: ownership is not editable). */
  workspaceId: string;
  /** The permission mode (A4): the session's three modes, same names, same
   *  default (manual). */
  permissionMode: AppCronPermissionMode;
  /** The task's display title; '' = the server-side untitled fallback (the
   *  wire key stays absent — never a locale string). */
  title: string;
  /** The model alias the task's runs bind; '' = the workspace's default
   *  model (the wire key stays absent). */
  model: string;
  /** The thinking effort the task's runs carry; '' = the model's catalog
   *  default. */
  effort: string;
}

/** The user's local IANA timezone (the visible default). */
export function localTimezone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || 'local';
  } catch {
    return 'local';
  }
}

export const EMPTY_CRON_DRAFT: CronCreateDraft = {
  schedule: null,
  prompt: '',
  timezone: '',
  maxFiresText: '',
  workspaceId: '',
  permissionMode: 'manual',
  title: '',
  model: '',
  effort: '',
};

/** A fresh draft with the timezone pre-filled as the user's local zone (the
 *  default is a VISIBLE value the user can see and change) and the page's
 *  workspace preset (A2: the workspace comes from the workspace list — the
 *  current one preselected). */
export function newCronDraft(workspaceId: string): CronCreateDraft {
  return {
    ...EMPTY_CRON_DRAFT,
    workspaceId,
    timezone: localTimezone(),
  };
}

/** The edit form's draft: the existing task pre-filled into the same shape
 *  the creation form edits (A9 / §5 — one form component, two modes).
 *  `fallbackTimezone` fills a task that was created without an explicit
 *  zone (the engine then evaluates it in the host's zone) so the prefilled
 *  form shows and previews the SAME zone the task actually runs in —
 *  never an empty picker that would silently re-anchor it. */
export function draftFromTask(task: AppCronTask, fallbackTimezone = ''): CronCreateDraft {
  return {
    // A task persisted before the structured picker (or one whose shape the
    // picker cannot reproduce) opens with the raw expression — the advanced
    // path stays honest rather than guessing a preset.
    schedule: scheduleFromTask(task),
    prompt: task.prompt,
    timezone: task.timezone?.trim() || fallbackTimezone,
    maxFiresText: task.maxFires !== undefined ? String(task.maxFires) : '',
    workspaceId: task.scope?.kind === 'workspace' ? task.scope.workspaceId : '',
    permissionMode: task.permissionMode ?? 'manual',
    title: task.title ?? '',
    model: task.model ?? '',
    effort: task.effort ?? '',
  };
}

/** The schedule the picker opens with for an existing task: its interval
 *  spec when it has one, else its expression in the advanced (raw) custom
 *  shape; the end date rides along when set. */
function scheduleFromTask(task: AppCronTask): CronSchedule | null {
  const ends =
    task.endsAt !== undefined && task.endsAt !== null && task.endsAt !== ''
      ? ({ kind: 'on', date: task.endsAt } as const)
      : ({ kind: 'never' } as const);
  if (task.interval) {
    return {
      kind: 'custom',
      mode: 'interval',
      // The draft holds the picker's shape (the anchor is the SUBMIT day,
      // re-derived on save — the task's own persisted anchor date is not an
      // editable field); keeping the persisted anchor here would silently
      // shift a task's phase on the next save.
      interval: {
        every: task.interval.every,
        unit: task.interval.unit,
        time: task.interval.time,
      },
      ends,
    };
  }
  if (task.cron.trim() === '') return null;
  return { kind: 'custom', mode: 'raw', raw: { expr: task.cron }, ends };
}

/** Whether the draft can be submitted: a complete schedule and a prompt are
 *  the required core; the optionals may stay empty (their wire keys are
 *  omitted — the engine reads absence as the documented defaults). */
export function cronDraftSubmittable(draft: CronCreateDraft): boolean {
  return (
    draft.schedule !== null &&
    scheduleComplete(draft.schedule) &&
    draft.prompt.trim() !== ''
  );
}

/** The recurring wire flag derived from the schedule's shape (SDR-110):
 *  the six presets are recurring; a custom schedule with an ends date is
 *  the one-off CANDIDATE — confirmed against the preview's t0 by comparing
 *  its day IN THE TASK TIMEZONE against ends.date. Without a t0 the
 *  candidate reads NOT recurring (the honest "not confirmed" state), and
 *  the server stays the authoritative validator. */
export function cronDraftRecurring(
  schedule: CronSchedule,
  previewT0?: number | null,
  timezone?: string,
): boolean {
  if (!isOneOffCandidate(schedule)) return true;
  if (previewT0 === null || previewT0 === undefined) return false;
  const { ends } = schedule as Extract<CronSchedule, { kind: 'custom' }>;
  return zonedDateOf(previewT0, timezone) !== (ends as { kind: 'on'; date: string }).date;
}

/** The calendar date of an epoch-ms instant in a named zone, `YYYY-MM-DD`.
 *  An empty/`local` zone falls back to the browser's local zone (the wire's
 *  `local` semantics). */
export function zonedDateOf(epochMs: number, timezone?: string): string {
  const zone = timezone?.trim() ?? '';
  if (zone === '' || zone === 'local') return localDateOf(epochMs);
  try {
    const parts = new Intl.DateTimeFormat('en-CA', {
      timeZone: zone,
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
    }).format(new Date(epochMs));
    return /^\d{4}-\d{2}-\d{2}$/.test(parts) ? parts : localDateOf(epochMs);
  } catch {
    // An unknown zone name degrades to the local day rather than throwing —
    // the server stays the authoritative validator.
    return localDateOf(epochMs);
  }
}

/** The local calendar date of an epoch-ms instant, `YYYY-MM-DD`. */
export function localDateOf(epochMs: number): string {
  const d = new Date(epochMs);
  const year = d.getFullYear();
  const month = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  return `${year}-${month}-${day}`;
}

/** Fold the draft into the create payload: empty optionals stay ABSENT
 *  (never null/0 — the engine's absent-key defaults apply), the budget
 *  parses only when the recurring flag is on and the text is a positive
 *  integer. The schedule compiles through `compileSchedule` — a cron shape
 *  rides the `cron` key, an interval shape rides the structured `interval`
 *  key (exactly one, SDR-113), and the inclusive end date rides `endsAt`
 *  when set. The permission mode rides VERBATIM (A4 — what was picked is
 *  what the run executes); the workspace is NOT part of the payload (the
 *  caller addresses it). */
export function cronDraftToInput(
  draft: CronCreateDraft,
  anchorDate?: string,
  previewT0?: number | null,
): AppCronTaskInput {
  if (draft.schedule === null) {
    throw new Error('cronDraftToInput: the draft carries no schedule');
  }
  const compiled = compileSchedule(
    draft.schedule,
    draft.timezone,
    // The explicit anchor lets tests pin the submit-day (the interval
    // spec's anchorDate) to a fixed clock instead of `new Date()`.
    anchorDate,
  );
  const maxFiresText = draft.maxFiresText.trim();
  const recurring = cronDraftRecurring(draft.schedule, previewT0, draft.timezone);
  const maxFires =
    recurring && maxFiresText !== '' ? Number.parseInt(maxFiresText, 10) : undefined;
  return {
    ...(compiled.type === 'cron'
      ? { cron: compiled.cron }
      : { interval: compiled.interval }),
    ...(compiled.endBoundary !== null ? { endsAt: compiled.endBoundary } : {}),
    prompt: draft.prompt,
    recurring,
    ...(recurring && Number.isFinite(maxFires) && maxFires !== undefined && maxFires > 0
      ? { maxFires }
      : {}),
    ...(draft.timezone.trim() !== '' ? { timezone: draft.timezone.trim() } : {}),
    permissionMode: draft.permissionMode,
    ...(draft.title.trim() !== '' ? { title: draft.title.trim() } : {}),
    ...(draft.model.trim() !== '' ? { model: draft.model.trim() } : {}),
    ...(draft.effort.trim() !== '' ? { effort: draft.effort.trim() } : {}),
  };
}

// ---------------------------------------------------------------------------
// The prediction timeline (A5 / B1 / B2)
// ---------------------------------------------------------------------------

/** One point on the prediction timeline. `past` points are the THEORETICAL
 *  past (derived from the rule, never a run record) — the UI labels them
 *  理论值 and renders them de-emphasised. */
export interface PredictionPoint {
  kind: 'past' | 'next' | 'nextNext';
  /** Epoch ms; null when the rule has no such point (a one-off's past, a
   *  one-off's second upcoming fire) — the UI renders 「—」. */
  at: number | null;
}

/** The rule's predicted past instant (A5): the last occurrence at or before
 *  NOW — the backend `previousT0` contract (`now` itself counts as past,
 *  `on` in the kernel). The frontend never evaluates occurrences itself
 *  (ADR-017): a null reads 「—」 (a one-off-shaped rule, an older engine, or
 *  a failed preview) rather than an invented value. */
export function rulePastInstant(
  preview: Pick<AppCronPreview, 'previousT0'> | null | undefined,
): number | null {
  return preview?.previousT0 ?? null;
}

/** The timeline model: past → next → next-next (B1). The past point is
 *  omitted for high-frequency rules (B1: an hourly rule's "an hour ago"
 *  carries no decision value) — the caller decides via `weakPast`/`hidePast`
 *  from the schedule kind. */
export interface PredictionTimeline {
  points: PredictionPoint[];
  /** The zone every point is rendered in (B2: the label rides each point). */
  timezone: string;
}

/** Build the timeline (A5/B1). `showPast = false` drops the past point for
 *  high-frequency rules; a one-off keeps its 「—」 past slot only while the
 *  caller wants it. */
export function predictionTimeline(
  preview: Pick<AppCronPreview, 'occurrences' | 'previousT0'> | null | undefined,
  options: { timezone?: string; showPast?: boolean } = {},
): PredictionTimeline {
  const [t0, t1] = preview?.occurrences ?? [];
  const points: PredictionPoint[] = [];
  if (options.showPast !== false) {
    points.push({ kind: 'past', at: rulePastInstant(preview) });
  }
  points.push({ kind: 'next', at: t0 ?? null });
  points.push({ kind: 'nextNext', at: t1 ?? null });
  return { points, timezone: options.timezone?.trim() ?? '' };
}

/** Whether a schedule kind is high-frequency (B1's weakening rule). */
export function isHighFrequencySchedule(kind: string | undefined): boolean {
  return kind === 'hourly';
}

/** The timezone label (B2): the `GMT+9`-style short offset of an instant in
 *  a zone, computed with an explicit timeZone so the label and the instant
 *  can never disagree (the host's zone never leaks in). The offset is
 *  normalised to the `GMT±H[:MM]` spelling (`GMT`, not `GMT+0`/`UTC`) so
 *  the label reads the same in every locale. An empty zone = the host's
 *  local zone, labelled `local`. */
export function zoneOffsetLabel(epochMs: number, timezone: string): string {
  const zone = timezone.trim();
  if (zone === '' || zone === 'local') return 'local';
  try {
    const parts = new Intl.DateTimeFormat('en-US', {
      timeZone: zone,
      timeZoneName: 'shortOffset',
    }).formatToParts(new Date(epochMs));
    const name = parts.find((part) => part.type === 'timeZoneName')?.value?.trim() ?? '';
    return normalizeOffsetLabel(name) || zone;
  } catch {
    return zone;
  }
}

/** `GMT+9` / `GMT+09:00` / `GMT+0` / `UTC` / `GMT` → one spelling: `GMT`,
 *  or `GMT±H[:MM]`. Anything else passes through verbatim. */
function normalizeOffsetLabel(name: string): string {
  const match = /^(?:GMT|UTC)(?:([+-])(\d{1,2})(?::?(\d{2}))?)?$/.exec(name);
  if (match === null) return name;
  const [, sign, hours, minutes] = match;
  if (sign === undefined) return 'GMT';
  const h = Number(hours);
  const m = minutes === undefined ? 0 : Number(minutes);
  if (h === 0 && m === 0) return 'GMT';
  return `GMT${sign}${h}${m === 0 ? '' : `:${String(m).padStart(2, '0')}`}`;
}

// ---------------------------------------------------------------------------
// Detail history (A5's 事实 side)
// ---------------------------------------------------------------------------

/** The most recent run's factual start (epoch ms) — 事实 comes only from run
 *  records, never from a prediction. Null when the task never really ran. */
export function lastRealRunAt(
  runs: ReadonlyArray<{ startedAt: string; status: string }>,
): number | null {
  let latest: number | null = null;
  for (const run of runs) {
    if (run.status === 'skipped') continue;
    const at = Date.parse(run.startedAt);
    if (Number.isNaN(at)) continue;
    if (latest === null || at > latest) latest = at;
  }
  return latest;
}
