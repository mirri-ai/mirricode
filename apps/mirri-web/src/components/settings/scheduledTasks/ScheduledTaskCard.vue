<!-- apps/mirri-web/src/components/settings/scheduledTasks/ScheduledTaskCard.vue -->
<!-- One scheduled task on the list page (§5 列表页基准): three layers — the
     bold title, the grey instruction (two lines, truncated), and the bottom
     metadata row (the repeat rule · next run in a distinguishable colour,
     plus the grey run count). Deliberately ZERO action buttons and no status
     badge: the whole card is one click into the detail view, where editing,
     pausing and history live. -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppCronTask } from '../../../api/types';
import { scheduledTaskRunCount, scheduledTaskStatus } from '../../../lib/cronManagement';
import { describeSchedule, type CronSchedule } from '../../../lib/cronSchedule';
import { humanizeCron } from '../../../lib/cronHumanize';

const props = defineProps<{
  task: AppCronTask;
  /** The parent's clock (epoch ms) — the relative next-run label reads it so
   *  one shared tick moves every card. */
  now?: number;
}>();

const emit = defineEmits<{ open: [] }>();

const { t, locale } = useI18n();

/** The task's title — the i18n untitled fallback (the server answers null,
 *  never a locale string). */
const title = computed(() => props.task.title ?? t('settings.scheduledTaskUntitled'));

/** The repeat rule in human words (A7: no 周期规则 jargon leaks into the
 *  card — 「每天 09:00」, not a cron expression; an expression the humanizer
 *  cannot read falls back to its raw spelling, the honest degrade). */
const scheduleText = computed(() => {
  const task = props.task;
  if (task.interval) {
    // SDR-116: every=1 day/week reads the natural word (每天/每周), not 「每 1 天」.
    if (task.interval.every === 1 && (task.interval.unit === 'day' || task.interval.unit === 'week')) {
      return t(
        task.interval.unit === 'day'
          ? 'settings.cronRuleDescIntervalEveryOne'
          : 'settings.cronRuleDescIntervalEveryOneWeek',
        { time: task.interval.time },
      );
    }
    return t('settings.cronRuleDescInterval', {
      every: task.interval.every,
      unit: t(`settings.cronRuleIntervalUnit_${task.interval.unit}`),
      time: task.interval.time,
    });
  }
  if (task.cron.trim() === '') return t('settings.scheduledTaskUntitled');
  const humanized = humanizeCron(task.cron, t);
  return humanized;
});

/** The preset-shaped description when the task's expression is one of the
 *  six presets (the card prefers the structured sentence over the generic
 *  humanizer); otherwise the humanizer's answer. */
const presetScheduleText = computed(() => {
  const parsed = presetFromExpression(props.task.cron);
  if (parsed === null) return null;
  return describeSchedule(parsed, props.task.timezone ?? '', t);
});

const frequencyText = computed(() => presetScheduleText.value ?? scheduleText.value);

/** The next-run clause: the soonest upcoming ideal occurrence, or an honest
 *  status word when nothing more will fire (§5: 下次 in a distinguishable
 *  colour). SDR-120: an ended task answers with the status word alone — no
 *  time slot, no dangling 「—」 — because 「已结束」 already means there is
 *  no next run. */
const nextText = computed(() => {
  const task = props.task;
  const status = scheduledTaskStatus(task);
  if (status === 'paused') return t('settings.scheduledTaskNextRunPaused');
  if (status === 'completed') return t('settings.scheduledTaskNextRunEnded');
  const at = task.nextFireAt ?? task.nextTwoFires?.[0] ?? null;
  if (at === null) return t('settings.scheduledTaskNextRunEnded');
  return t('settings.scheduledTaskNextRun', { time: relativeTime(at) });
});

/** A short relative distance (「12 小时后」) — the card's compact unit. */
function relativeTime(at: number): string {
  const diff = at - (props.now ?? Date.now());
  if (diff <= 0) return t('settings.scheduledTaskNextRunSoon');
  const rtf = new Intl.RelativeTimeFormat(locale.value, { numeric: 'auto' });
  const minutes = Math.round(diff / 60_000);
  if (minutes < 60) return rtf.format(minutes, 'minute');
  const hours = Math.round(minutes / 60);
  if (hours < 48) return rtf.format(hours, 'hour');
  return rtf.format(Math.round(hours / 24), 'day');
}

const runCount = computed(() => scheduledTaskRunCount(props.task));

/** SDR-120 / VU25: a never-run task says so plainly instead of 「已运行 0 次」. */
const runCountText = computed(() =>
  runCount.value === 0
    ? t('settings.scheduledTaskRunCountZero')
    : t('settings.scheduledTaskRunCount', { count: runCount.value }),
);

const endsSuffix = computed(() =>
  props.task.endsAt ? t('settings.scheduledTaskEndsAt', { date: props.task.endsAt }) : '',
);

/** Recognize the six presets' expressions so the card can show the same
 *  structured sentence the form built (compile in reverse, tolerantly:
 *  anything else returns null and the humanizer speaks instead). */
function presetFromExpression(expr: string): CronSchedule | null {
  const fields = expr.trim().split(/\s+/);
  if (fields.length !== 5) return null;
  const [minute, hour, dom, month, dow] = fields as [string, string, string, string, string];
  if (!/^\d+$/.test(minute)) return null;
  const pad = (value: string) => String(Number(value)).padStart(2, '0');
  if (hour === '*' && dom === '*' && month === '*' && dow === '*') {
    return { kind: 'hourly', minute: Number(minute) };
  }
  if (!/^\d+$/.test(hour)) return null;
  const time = `${pad(hour)}:${pad(minute)}`;
  if (month !== '*') return null;
  if (dom === '*' && dow === '*') return { kind: 'daily', time };
  if (dom === '*' && dow === '1-5') return { kind: 'weekday', time };
  if (dom === '*' && /^\d(,\d)*$/.test(dow)) {
    return { kind: 'weekly', days: dow.split(',').map(Number), time };
  }
  if (/^\d+$/.test(dom) && dow === '*') return { kind: 'monthly', day: Number(dom), time };
  return null;
}
</script>

<template>
  <button type="button" class="task-card" :data-task-id="task.id" @click="emit('open')">
    <span class="task-card-title">{{ title }}</span>
    <span class="task-card-prompt">{{ task.prompt }}</span>
    <span class="task-card-meta">
      <span class="task-card-frequency">
        <span class="task-card-frequency-rule">{{ frequencyText }}</span>
        <span class="task-card-dot" aria-hidden="true">·</span>
        <span class="task-card-next">{{ nextText }}</span>
        <template v-if="endsSuffix">
          <span class="task-card-dot" aria-hidden="true">·</span>
          <span class="task-card-ends">{{ endsSuffix }}</span>
        </template>
      </span>
      <span class="task-card-count">{{ runCountText }}</span>
    </span>
  </button>
</template>

<style scoped>
.task-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  align-items: stretch;
  text-align: left;
  width: 100%;
  min-height: 132px;
  padding: var(--space-4);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  background: var(--color-surface);
  color: var(--color-text);
  cursor: pointer;
  transition: border-color var(--duration-fast) var(--ease-out),
    background var(--duration-fast) var(--ease-out);
}
.task-card:hover { border-color: var(--color-line-strong); background: var(--color-surface-raised); }
.task-card:focus-visible { outline: none; box-shadow: var(--p-focus-ring); }
.task-card-title {
  font-family: var(--font-ui);
  font-size: var(--text-base);
  font-weight: var(--weight-semibold);
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.task-card-prompt {
  flex: 1;
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  line-height: var(--leading-normal);
  color: var(--color-text-muted);
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  word-break: break-word;
}
/* SDR-119: the facts read as one sentence — frequency · next · ends · runs
   clustered from the left, not pinned to both edges of the card. */
.task-card-meta {
  display: flex;
  align-items: baseline;
  gap: var(--space-3);
  min-width: 0;
}
.task-card-frequency {
  display: inline-flex;
  align-items: baseline;
  gap: 4px;
  min-width: 0;
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* §5: the next-run clause is the one coloured fact on the card. */
.task-card-next { color: var(--color-success); }
.task-card-ends { color: var(--color-text-faint); }
.task-card-dot { color: var(--color-text-faint); }
.task-card-count {
  font-size: var(--text-xs);
  color: var(--color-text-faint);
  font-variant-numeric: tabular-nums;
}
</style>
