<!-- apps/mirri-web/src/components/settings/CronPanel.vue -->
<!-- Cron jobs management panel (B6). Renders inside the Settings dialog's
     "cron" tab. Self-fetching (SkillsPanel mode): the rows come from the
     v3 rust engine's cron.* RPCs directly, scoped to the ACTIVE SESSION —
     cron tasks live on the session, so with no session open the panel
     degrades to an honest empty state instead of inventing data. Management
     (list / run-now / delete) is v3-only (backend-gated banner otherwise);
     creating a task stays with the model's CronCreate tool (D-6). -->
<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppCronTask } from '../../api/types';
import { getMirriWebApi } from '../../api';
import { useMirriWebClient } from '../../composables/useMirriWebClient';
import { useConfirmDialog } from '../../composables/useConfirmDialog';
import { cronTaskDueState, filterCronTasks } from '../../lib/cronManagement';
import Badge from '../ui/Badge.vue';
import Banner from '../ui/Banner.vue';
import Button from '../ui/Button.vue';
import EmptyState from '../ui/EmptyState.vue';
import Icon from '../ui/Icon.vue';
import Spinner from '../ui/Spinner.vue';
import ToolbarSearch from '../ui/ToolbarSearch.vue';

const { t, locale } = useI18n();
const { confirm } = useConfirmDialog();
const api = getMirriWebApi();
const client = useMirriWebClient();
const { backend, activeSessionId } = client;

// -------------------------------------------------------------------------
// State
// -------------------------------------------------------------------------
const tasks = ref<AppCronTask[]>([]);
const loading = ref(false);
const error = ref('');
const searchQuery = ref('');
/** The row whose detail pane is open (master-detail). */
const selectedId = ref<string | null>(null);
/** The row with an in-flight run-now ack (inline spinner). */
const triggeringId = ref<string | null>(null);
/** The row with an in-flight delete ack. */
const deletingId = ref<string | null>(null);

// Cron management rides the v3 rust engine's cron.* RPCs (B6 D-4); the
// v1/v2 REST daemon has no cron routes and gets an honest hint instead.
const isV3 = computed(() => backend.value === 'v3');
const hasSession = computed(() => activeSessionId.value !== '');
const filtered = computed(() => filterCronTasks(tasks.value, searchQuery.value));
const selectedTask = computed<AppCronTask | undefined>(() =>
  selectedId.value === null
    ? undefined
    : tasks.value.find((task) => task.id === selectedId.value),
);

// -------------------------------------------------------------------------
// Load — re-run when the session (the cron scope) or the backend changes,
// so the dev backend switcher refetches and the v3 gate re-evaluates.
// -------------------------------------------------------------------------
/** Monotonic load token: a response from a superseded load (session/backend
 *  switched mid-flight) is discarded instead of overwriting newer rows. */
let loadSeq = 0;

async function load(opts: { silent?: boolean } = {}): Promise<void> {
  const seq = ++loadSeq;
  error.value = '';
  if (!isV3.value) {
    tasks.value = [];
    selectedId.value = null;
    loading.value = false;
    return;
  }
  const sid = activeSessionId.value;
  if (!sid) {
    tasks.value = [];
    selectedId.value = null;
    loading.value = false;
    return;
  }
  // A silent refresh (the run-now re-confirmation) keeps the current rows on
  // screen instead of flashing the full-panel spinner.
  if (!opts.silent) loading.value = true;
  try {
    const rows = await api.listCronTasks(sid);
    if (seq !== loadSeq) return;
    tasks.value = rows;
  } catch (e) {
    if (seq === loadSeq) error.value = e instanceof Error ? e.message : String(e);
  } finally {
    if (seq === loadSeq) loading.value = false;
  }
}

watch(
  () => `${backend.value}:${activeSessionId.value ?? ''}`,
  () => { void load(); },
  { immediate: true },
);

// -------------------------------------------------------------------------
// Row presentation
// -------------------------------------------------------------------------
/** Epoch-ms → locale-formatted date+time; missing values stay honest ('—'). */
function formatTime(epochMs: number | null | undefined): string {
  if (epochMs === null || epochMs === undefined) return t('settings.cronNever');
  const d = new Date(epochMs);
  if (Number.isNaN(d.getTime())) return t('settings.cronNever');
  return new Intl.DateTimeFormat(locale.value, { dateStyle: 'medium', timeStyle: 'short' }).format(d);
}

function dueState(task: AppCronTask): 'future' | 'due' | 'dormant' {
  // Snapshot at render time — the bucket refreshes on the next re-render
  // (search/selection/data change), not on a wall-clock timer.
  return cronTaskDueState(task.nextFireAt, Date.now());
}

// -------------------------------------------------------------------------
// Row actions
// -------------------------------------------------------------------------
async function onTrigger(task: AppCronTask): Promise<void> {
  const sid = activeSessionId.value;
  if (!sid) return;
  triggeringId.value = task.id;
  try {
    await api.triggerCronTask(sid, task.id);
    // Run-now leaves the schedule untouched (B6 D-2) — the silent refresh
    // only re-confirms the row (and refreshes lastFiredAt once the engine
    // reports it) without flashing the full-panel spinner.
    await load({ silent: true });
  } catch (e) {
    client.pushOperationFailure('triggerCronTask', e);
  } finally {
    triggeringId.value = null;
  }
}

async function onDelete(task: AppCronTask): Promise<void> {
  const sid = activeSessionId.value;
  if (!sid) return;
  const ok = await confirm({
    title: t('settings.cronConfirmDeleteTitle'),
    message: t('settings.cronConfirmDeleteBody'),
    variant: 'danger',
  });
  if (!ok) return;
  deletingId.value = task.id;
  try {
    await api.deleteCronTask(sid, task.id);
    tasks.value = tasks.value.filter((row) => row.id !== task.id);
    if (selectedId.value === task.id) selectedId.value = null;
  } catch (e) {
    client.pushOperationFailure('deleteCronTask', e);
  } finally {
    deletingId.value = null;
  }
}
</script>

<template>
  <div class="cron-panel">
    <!-- Header -->
    <div class="panel-head">
      <div class="panel-kicker">{{ t('settings.tabs.cron') }}</div>
      <h4 class="panel-title">{{ t('settings.cronTitle') }}</h4>
      <p class="panel-desc">{{ t('settings.cronDesc') }}</p>
    </div>

    <!-- Toolbar -->
    <div class="toolbar">
      <ToolbarSearch
        v-model="searchQuery"
        :placeholder="t('settings.cronSearch')"
        :aria-label="t('settings.cronSearch')"
      />
      <Button variant="secondary" size="sm" :disabled="!isV3 || !hasSession || loading" @click="load()">
        {{ t('settings.cronRefresh') }}
      </Button>
    </div>

    <div v-if="error" class="error-msg">{{ error }}</div>

    <!-- v3-only honest degrade: no fake rows on the v1/v2 REST face -->
    <Banner v-if="!isV3" variant="info">{{ t('settings.cronV3Only') }}</Banner>

    <EmptyState v-else-if="!hasSession" :title="t('settings.cronNoSession')" />

    <EmptyState v-else-if="loading">
      <Spinner />
    </EmptyState>

    <EmptyState
      v-else-if="filtered.length === 0 && searchQuery.trim() !== ''"
      :title="t('settings.cronNoMatch', { query: searchQuery.trim() })"
    />

    <EmptyState
      v-else-if="filtered.length === 0"
      :title="t('settings.cronEmpty')"
      :hint="t('settings.cronEmptyHint')"
    >
      <template #icon><Icon name="clock" /></template>
    </EmptyState>

    <div v-else class="master-detail">
      <!-- Left: task list -->
      <div class="list-pane" :class="{ 'hidden-mobile': selectedTask !== undefined }">
        <div class="list-scroll">
          <button
            v-for="task in filtered"
            :key="task.id"
            type="button"
            class="list-row"
            :class="{ selected: selectedId === task.id }"
            :aria-current="selectedId === task.id ? 'true' : undefined"
            @click="selectedId = task.id"
          >
            <div class="list-row-main">
              <div class="list-row-header">
                <span class="list-row-cron" :title="task.cron">{{ task.cron }}</span>
                <div class="list-row-badges">
                  <Badge :variant="task.recurring ? 'info' : 'neutral'" size="sm">
                    {{ task.recurring ? t('settings.cronRecurring') : t('settings.cronOneShot') }}
                  </Badge>
                  <Badge variant="neutral" size="sm">{{ task.timezone ?? t('settings.cronTimezoneLocal') }}</Badge>
                  <Badge v-if="task.stale" variant="warning" size="sm">{{ t('settings.cronStale') }}</Badge>
                  <Badge v-if="dueState(task) === 'due'" variant="warning" size="sm">{{ t('settings.cronDue') }}</Badge>
                </div>
              </div>
              <span class="list-row-prompt" :title="task.prompt">{{ task.prompt }}</span>
              <span class="list-row-next">{{ t('settings.cronNextFire') }}: {{ formatTime(task.nextFireAt) }}</span>
            </div>
          </button>
        </div>
      </div>

      <!-- Right: detail pane (selected row) -->
      <div class="detail-pane" :class="{ 'hidden-mobile': selectedTask === undefined }">
        <div v-if="selectedTask === undefined" class="empty-state">
          <Icon name="clock" size="lg" />
          <p>{{ t('settings.cronSelectPrompt') }}</p>
        </div>

        <template v-else>
          <div class="detail-header">
            <button type="button" class="back-btn" @click="selectedId = null">
              <Icon name="arrow-left" size="sm" />
              <span>{{ t('settings.cronBackToList') }}</span>
            </button>
            <span class="detail-cron">{{ selectedTask.cron }}</span>
          </div>
          <div class="detail-scroll">
            <div class="detail-field">
              <span class="detail-label">{{ t('settings.cronPrompt') }}</span>
              <pre class="detail-prompt">{{ selectedTask.prompt }}</pre>
            </div>
            <div class="detail-grid">
              <div class="detail-field">
                <span class="detail-label">{{ t('settings.cronSchedule') }}</span>
                <span class="detail-value mono">{{ selectedTask.cron }}</span>
              </div>
              <div class="detail-field">
                <span class="detail-label">{{ t('settings.cronTimezone') }}</span>
                <span class="detail-value mono">{{ selectedTask.timezone ?? t('settings.cronTimezoneLocal') }}</span>
              </div>
              <div class="detail-field">
                <span class="detail-label">{{ t('settings.cronRecurring') }}</span>
                <span class="detail-value">
                  {{ selectedTask.recurring ? t('settings.cronRecurring') : t('settings.cronOneShot') }}
                </span>
              </div>
              <div class="detail-field">
                <span class="detail-label">{{ t('settings.cronCreatedAt') }}</span>
                <span class="detail-value">{{ formatTime(selectedTask.createdAt) }}</span>
              </div>
              <div class="detail-field">
                <span class="detail-label">{{ t('settings.cronLastFired') }}</span>
                <span class="detail-value">{{ formatTime(selectedTask.lastFiredAt) }}</span>
              </div>
              <div class="detail-field">
                <span class="detail-label">{{ t('settings.cronNextFire') }}</span>
                <span class="detail-value">{{ formatTime(selectedTask.nextFireAt) }}</span>
              </div>
            </div>
            <div class="detail-actions">
              <Button
                variant="secondary"
                size="sm"
                :disabled="triggeringId === selectedTask.id"
                @click="onTrigger(selectedTask)"
              >
                <Spinner v-if="triggeringId === selectedTask.id" size="sm" />
                {{ t('settings.cronTrigger') }}
              </Button>
              <Button
                variant="danger-soft"
                size="sm"
                :disabled="deletingId === selectedTask.id"
                @click="onDelete(selectedTask)"
              >
                {{ t('settings.cronDelete') }}
              </Button>
            </div>
          </div>
        </template>
      </div>
    </div>
  </div>
</template>

<style scoped>
.cron-panel { display: flex; flex-direction: column; height: 100%; gap: var(--space-3); }

.panel-head { margin-bottom: var(--space-2); }
.panel-kicker { font-size: var(--text-xs); letter-spacing: 0.08em; text-transform: uppercase; color: var(--color-text-faint); margin-bottom: var(--space-1); }
.panel-title { margin: 0 0 var(--space-2); font-family: var(--font-ui); font-size: var(--text-2xl); font-weight: var(--weight-semibold); letter-spacing: -0.01em; color: var(--color-text); }
.panel-desc { margin: 0; font-family: var(--font-ui); font-size: var(--text-sm); line-height: var(--leading-normal); color: var(--color-text-muted); max-width: 560px; }

.toolbar { display: flex; align-items: center; gap: var(--space-3); flex-wrap: wrap; }

.error-msg { padding: var(--space-2) var(--space-3); border-radius: var(--radius-md); background: var(--color-danger-soft); color: var(--color-danger); font-size: var(--text-sm); }

.master-detail {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(280px, 0.9fr);
  gap: var(--space-3);
  min-height: 0;
}

/* --- Left list pane --- */
.list-pane {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  overflow: hidden;
  min-height: 0;
}

.list-scroll {
  flex: 1;
  overflow-y: auto;
  padding: var(--space-1);
}

.list-row {
  display: flex;
  width: 100%;
  padding: var(--space-2) var(--space-3);
  border: none;
  border-radius: var(--radius-md);
  background: transparent;
  cursor: pointer;
  text-align: left;
  transition: background var(--duration-fast) var(--ease-out);
}
.list-row:hover { background: var(--color-surface-sunken); }
.list-row.selected { background: var(--color-accent-soft); }

.list-row-main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.list-row-header {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  min-width: 0;
}
.list-row-cron {
  font-family: var(--font-mono);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text);
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.list-row-badges {
  display: flex;
  gap: 4px;
  flex-wrap: wrap;
  min-width: 0;
}

.list-row-prompt {
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.list-row-next {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: var(--color-text-faint);
}

/* --- Right detail pane --- */
.detail-pane {
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  overflow: hidden;
  display: flex;
  flex-direction: column;
  min-height: 0;
}

.detail-header {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  border-bottom: 1px solid var(--color-line);
}

.back-btn {
  display: flex;
  align-items: center;
  gap: 4px;
  border: none;
  background: transparent;
  cursor: pointer;
  color: var(--color-text-muted);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  padding: 2px 4px;
  border-radius: var(--radius-sm);
  transition: color var(--duration-fast) var(--ease-out);
}
.back-btn:hover { color: var(--color-text); }
.back-btn:focus-visible { outline: none; box-shadow: var(--p-focus-ring); }

.detail-cron {
  font-family: var(--font-mono);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text);
}

.detail-scroll {
  flex: 1;
  overflow-y: auto;
  padding: var(--space-3);
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}

.detail-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
  gap: var(--space-3);
}

.detail-field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  min-width: 0;
}
.detail-label {
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  letter-spacing: 0.06em;
  text-transform: uppercase;
  color: var(--color-text-faint);
}
.detail-value {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-text);
}
.detail-value.mono { font-family: var(--font-mono); }

.detail-prompt {
  margin: 0;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface-sunken);
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  line-height: var(--leading-normal);
  color: var(--color-text);
  white-space: pre-wrap;
  word-break: break-word;
}

.detail-actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}

/* --- Empty state (detail pane) --- */
.empty-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: var(--space-3);
  height: 100%;
  color: var(--color-text-faint);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
}

/* --- Mobile --- */
@media (max-width: 640px) {
  .master-detail {
    grid-template-columns: 1fr;
  }
  .hidden-mobile {
    display: none;
  }
}
</style>
