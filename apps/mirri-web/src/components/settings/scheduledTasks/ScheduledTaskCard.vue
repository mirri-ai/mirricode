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
import {
  draftFromTask,
  scheduledTaskRunCount,
  scheduledTaskStatus,
} from '../../../lib/cronManagement';
import { describeScheduleRule } from '../../../lib/cronSchedule';

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

/** The repeat rule in human words. The card reads the SAME projection the
 *  edit form reopens with (`draftFromTask` → `describeScheduleRule`), so what
 *  the preview says while typing is exactly what the card says afterwards: a
 *  preset reads 「每天 09:00 执行」, an every-minute expression reads
 *  「每分钟」, and only a rule with no structured equivalent keeps its
 *  expression. The zone is NOT part of the sentence — the form shows it as
 *  its own clickable chip, and repeating it here would push the next-run
 *  clause out of the card's single-line metadata row. */
const schedule = computed(() => draftFromTask(props.task).schedule);

const frequencyText = computed(() =>
  schedule.value === null
    ? t('settings.scheduledTaskUntitled')
    : describeScheduleRule(schedule.value, t),
);

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
