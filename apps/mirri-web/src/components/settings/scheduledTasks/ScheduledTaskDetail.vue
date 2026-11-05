<!-- apps/mirri-web/src/components/settings/scheduledTasks/ScheduledTaskDetail.vue -->
<!-- One scheduled task's detail view (§5 详情页形态): the title with its
     status on top, then two tabs — 设置 (the SAME form component, prefilled —
     A9: edit in place, the workspace read-only) and 历史 (the REAL run
     records from `cron.runs.list`; predictions never appear here, facts are
     never predicted). Pause / resume and delete live in the settings tab's
     toolbar; run-now sits beside them. -->
<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppCronRun, AppCronTask, AppModel } from '../../../api/types';
import { getMirriWebApi } from '../../../api';
import { useMirriWebClient } from '../../../composables/useMirriWebClient';
import { useConfirmDialog } from '../../../composables/useConfirmDialog';
import {
  draftFromTask,
  lastRealRunAt,
  localTimezone,
  scheduledTaskStatus,
  type CronCreateDraft,
} from '../../../lib/cronManagement';
import Badge from '../../ui/Badge.vue';
import Button from '../../ui/Button.vue';
import EmptyState from '../../ui/EmptyState.vue';
import Spinner from '../../ui/Spinner.vue';
import Tabs from '../../ui/Tabs.vue';
import ScheduledTaskForm from './ScheduledTaskForm.vue';

const props = defineProps<{
  task: AppCronTask;
  workspaceId: string;
  workspaces: Array<{ id: string; name: string }>;
  models: AppModel[];
  /** The parent's (submitting) flag for the edit save. */
  saving: boolean;
  /** Bumped by the parent after any mutation so the history reloads. */
  refreshKey: number;
}>();

const emit = defineEmits<{
  back: [];
  save: [draft: CronCreateDraft, previewT0: number | null];
  togglePause: [];
  delete: [];
}>();

const { t, locale } = useI18n();
const api = getMirriWebApi();
const client = useMirriWebClient();
const { confirm } = useConfirmDialog();

const tab = ref<'settings' | 'history'>('settings');

const status = computed(() => scheduledTaskStatus(props.task));
const title = computed(() => props.task.title ?? t('settings.scheduledTaskUntitled'));

/** The edit form's initial draft — rebuilt from the task each time the task
 *  row changes (a save or pause answers the fresh row). */
const initialDraft = computed(() => draftFromTask(props.task, localTimezone()));

// -------------------------------------------------------------------------
// History — the real run records (A5: 事实 only)
// -------------------------------------------------------------------------
const runs = ref<AppCronRun[]>([]);
const runsLoading = ref(false);
let runsSeq = 0;

async function loadRuns(): Promise<void> {
  const seq = ++runsSeq;
  runsLoading.value = true;
  try {
    const rows = await api.listCronRuns(props.workspaceId, props.task.id);
    if (seq !== runsSeq) return;
    runs.value = rows;
  } catch (error) {
    if (seq === runsSeq) client.pushOperationFailure('listCronRuns', error);
  } finally {
    if (seq === runsSeq) runsLoading.value = false;
  }
}

watch(
  () => `${props.task.id}:${props.refreshKey}`,
  () => void loadRuns(),
  { immediate: true },
);

const lastRealRun = computed(() => lastRealRunAt(runs.value));

function formatTime(epochMs: number): string {
  return new Intl.DateTimeFormat(locale.value, { dateStyle: 'medium', timeStyle: 'short' }).format(
    new Date(epochMs),
  );
}

function formatRunTime(iso: string | null | undefined): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '—';
  return new Intl.DateTimeFormat(locale.value, { dateStyle: 'medium', timeStyle: 'short' }).format(d);
}

function runStatusLabel(status: AppCronRun['status']): string {
  switch (status) {
    case 'succeeded':
      return t('settings.scheduledTaskRunSucceeded');
    case 'failed':
      return t('settings.scheduledTaskRunFailed');
    case 'running':
      return t('settings.scheduledTaskRunRunning');
    case 'waiting_approval':
      return t('settings.scheduledTaskRunWaitingApproval');
    case 'rejected':
      return t('settings.scheduledTaskRunRejected');
    default:
      return t('settings.scheduledTaskRunSkipped');
  }
}

function runStatusVariant(status: AppCronRun['status']): 'success' | 'danger' | 'warning' | 'neutral' {
  switch (status) {
    case 'succeeded':
      return 'success';
    case 'failed':
      return 'danger';
    case 'running':
    case 'waiting_approval':
      return 'warning';
    default:
      return 'neutral';
  }
}

function runDurationLabel(run: AppCronRun): string {
  if (run.durationMs === null || run.durationMs === undefined) return '—';
  const seconds = Math.round(run.durationMs / 1000);
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

/** The explicit-open path (the user opening a run materializes its
 *  execution session — idempotent). The execution session is not in the
 *  sidebar list (the list hides sessions without a user prompt), so it is
 *  fetched into the list before it is selected — selecting an unlisted
 *  session renders nothing, and a run parked on an approval would be
 *  undecidable. */
async function openRun(run: AppCronRun): Promise<void> {
  if (!run.sessionRef) return;
  try {
    await api.openCronRun(props.workspaceId, run.sessionRef);
    await client.openSessionById(run.sessionRef);
  } catch (error) {
    client.pushOperationFailure('openCronRun', error);
  }
}

async function onDelete(): Promise<void> {
  const ok = await confirm({
    title: t('settings.scheduledTaskConfirmDeleteTitle'),
    message: t('settings.scheduledTaskConfirmDeleteBody'),
    variant: 'danger',
  });
  if (!ok) return;
  emit('delete');
}

const tabOptions = computed(() => [
  { value: 'settings', label: t('settings.scheduledTaskTabSettings') },
  { value: 'history', label: t('settings.scheduledTaskTabHistory') },
]);
</script>

<template>
  <div class="std">
    <div class="std-head">
      <button type="button" class="std-back" @click="emit('back')">
        {{ t('settings.scheduledTaskBackToList') }}
      </button>
      <h3 class="std-title">{{ title }}</h3>
      <span class="std-head-meta">
        <Badge v-if="status === 'paused'" variant="warning" size="sm">
          {{ t('settings.scheduledTaskPausedBadge') }}
        </Badge>
        <Badge v-else-if="status === 'completed'" variant="success" size="sm">
          {{ t('settings.scheduledTaskNextRunEnded') }}
        </Badge>
        <Button
          variant="ghost"
          size="sm"
          :disabled="saving"
          @click="emit('togglePause')"
        >{{ task.paused ? t('settings.scheduledTaskResume') : t('settings.scheduledTaskPause') }}</Button>
        <Button variant="danger-soft" size="sm" :disabled="saving" @click="onDelete">
          {{ t('settings.scheduledTaskDelete') }}
        </Button>
      </span>
    </div>

    <Tabs v-model="tab" :options="tabOptions" />

    <div v-if="tab === 'settings'" class="std-body">
      <ScheduledTaskForm
        :key="`${task.id}:${refreshKey}`"
        :initial-draft="initialDraft"
        mode="edit"
        :workspaces="workspaces"
        :models="models"
        :submitting="saving"
        @submit="(draft, previewT0) => emit('save', draft, previewT0)"
        @cancel="emit('back')"
      />
    </div>

    <div v-else class="std-body">
      <p class="std-fact-line">
        <span v-if="lastRealRun !== null">
          {{ t('settings.scheduledTaskHistoryLastRealRun', { time: formatTime(lastRealRun) }) }}
        </span>
        <span v-else>{{ t('settings.scheduledTaskHistoryEmpty') }}</span>
        <span class="std-fact-hint">{{ t('settings.scheduledTaskHistoryFactHint') }}</span>
      </p>
      <EmptyState v-if="runsLoading && runs.length === 0">
        <Spinner />
      </EmptyState>
      <EmptyState v-else-if="runs.length === 0" :title="t('settings.scheduledTaskHistoryEmpty')" />
      <ul v-else class="std-runs">
        <li v-for="run in runs" :key="run.runId" class="std-run">
          <div class="std-run-head">
            <Badge :variant="runStatusVariant(run.status)" size="sm">{{ runStatusLabel(run.status) }}</Badge>
            <span v-if="run.trigger" class="std-run-trigger">{{ t(`settings.scheduledTaskRunTrigger_${run.trigger}`) }}</span>
            <span v-if="run.launchDelayMs != null && run.launchDelayMs >= 60000" class="std-run-delay">
              {{ t('settings.scheduledTaskRunLaunchDelay', { minutes: Math.round(run.launchDelayMs / 60000) }) }}
            </span>
            <span class="std-run-time">{{ formatRunTime(run.startedAt) }}</span>
            <span class="std-run-duration">{{ runDurationLabel(run) }}</span>
            <Button
              v-if="run.sessionRef"
              variant="ghost"
              size="sm"
              @click="openRun(run)"
            >{{ t('settings.scheduledTaskRunOpen') }}</Button>
          </div>
          <p v-if="run.usage?.summary" class="std-run-summary">{{ run.usage.summary }}</p>
          <p v-if="run.skipReason" class="std-run-skip">{{ run.skipReason }}</p>
        </li>
      </ul>
    </div>
  </div>
</template>

<style scoped>
.std { display: flex; flex-direction: column; gap: var(--space-4); min-height: 0; flex: 1; overflow-y: auto; }
.std-head {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: var(--space-3);
  padding-top: var(--space-2);
}
.std-back {
  border: none;
  background: transparent;
  color: var(--color-text-muted);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  cursor: pointer;
  padding: 0;
}
.std-back:hover { color: var(--color-text); }
.std-title {
  margin: 0;
  font-family: var(--font-ui);
  font-size: var(--text-xl);
  font-weight: var(--weight-semibold);
  color: var(--color-text);
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.std-head-meta { display: inline-flex; align-items: center; gap: var(--space-2); margin-left: auto; }
.std-body { display: flex; flex-direction: column; min-height: 0; }
.std-fact-line { display: flex; flex-wrap: wrap; gap: var(--space-2); margin: 0; font-size: var(--text-sm); color: var(--color-text); }
.std-fact-hint { color: var(--color-text-faint); font-size: var(--text-xs); }
.std-runs { display: flex; flex-direction: column; gap: var(--space-2); margin: 0; padding: 0; list-style: none; }
.std-run { display: flex; flex-direction: column; gap: var(--space-1); padding: var(--space-2) 0; border-bottom: 1px solid var(--color-line); }
.std-run-head { display: flex; align-items: center; flex-wrap: wrap; gap: var(--space-2); }
.std-run-time { font-family: var(--font-mono); font-size: var(--text-xs); color: var(--color-text-muted); }
.std-run-trigger { font-size: var(--text-xs); color: var(--color-text-muted); }
.std-run-delay { font-size: var(--text-xs); color: var(--color-warning, var(--color-text-muted)); }
.std-run-duration { font-family: var(--font-mono); font-size: var(--text-xs); color: var(--color-text-faint); }
.std-run-summary { margin: 0; font-size: var(--text-xs); color: var(--color-text-muted); }
.std-run-skip { margin: 0; font-size: var(--text-xs); color: var(--color-text-faint); }
</style>
