<!-- apps/mirri-web/src/components/settings/TasksOverviewPanel.vue -->
<!-- Workspace task overview (B7 / P-023): the workspace-aggregated list of
     every live session's background tasks, rendered inside the Settings
     dialog's "tasks" tab. Self-fetching (CronPanel mode): rows come from the
     v3 rust engine's task.listWorkspace RPC on a 2s poll (paused while the
     tab is hidden), session titles are joined client-side against the
     session list the sidebar holds. Running rows stop via task.cancel behind
     a confirmation dialog (US12.1). v3-only (backend-gated banner). -->
<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppSession, AppTask, AppTaskStatus } from '../../api/types';
import { getMirriWebApi } from '../../api';
import { useMirriWebClient } from '../../composables/useMirriWebClient';
import { useConfirmDialog } from '../../composables/useConfirmDialog';
import {
  buildWorkspaceTaskRows,
  countByStatus,
  filterWorkspaceTasks,
  type WorkspaceTaskRow,
} from '../../lib/taskOverview';
import Badge from '../ui/Badge.vue';
import Banner from '../ui/Banner.vue';
import Button from '../ui/Button.vue';
import EmptyState from '../ui/EmptyState.vue';
import Icon from '../ui/Icon.vue';
import SegmentedControl from '../ui/SegmentedControl.vue';
import Spinner from '../ui/Spinner.vue';
import ToolbarSearch from '../ui/ToolbarSearch.vue';

const POLL_INTERVAL_MS = 2000;

const { t, locale } = useI18n();
const { confirm } = useConfirmDialog();
const api = getMirriWebApi();
const client = useMirriWebClient();
const { backend, appSessions } = client;

// -------------------------------------------------------------------------
// State
// -------------------------------------------------------------------------
const tasks = ref<ReadonlyArray<AppTask>>([]);
const loading = ref(false);
const error = ref('');
const searchQuery = ref('');
const statusFilter = ref<'all' | 'running'>('all');
/** The row with an in-flight stop ack (inline spinner). */
const stoppingId = ref<string | null>(null);
/** 1-second clock so a running row's elapsed timer keeps counting. */
const taskClock = ref(0);

// task.listWorkspace rides the v3 rust engine's RPC (B7 D-6); the v1/v2
// REST daemon has no workspace-task routes and gets an honest hint instead.
const isV3 = computed(() => backend.value === 'v3');

const sessionsById = computed(() => {
  const map = new Map<string, AppSession>();
  for (const s of appSessions.value) map.set(s.id, s);
  return map;
});

const rows = computed(() => buildWorkspaceTaskRows(tasks.value, sessionsById.value));
const filtered = computed(() =>
  filterWorkspaceTasks(rows.value, statusFilter.value, searchQuery.value),
);
// Counters describe the FILTERED set — the header numbers must match the rows
// the list below actually shows.
const counts = computed(() => countByStatus(filtered.value));

/** Rows grouped by session, preserving the server's ordering (sessions by
 *  recency, tasks by registration) — the group key is the first-seen
 *  position of each session. */
const grouped = computed<Array<{ sessionId: string; sessionTitle: string; archived: boolean; rows: WorkspaceTaskRow[] }>>(() => {
  const groups: Array<{ sessionId: string; sessionTitle: string; archived: boolean; rows: WorkspaceTaskRow[] }> = [];
  const bySession = new Map<string, (typeof groups)[number]>();
  for (const row of filtered.value) {
    let group = bySession.get(row.sessionId);
    if (group === undefined) {
      group = { sessionId: row.sessionId, sessionTitle: row.sessionTitle, archived: row.archived, rows: [] };
      bySession.set(row.sessionId, group);
      groups.push(group);
    }
    group.rows.push(row);
  }
  return groups;
});

const filterOptions = computed(() => [
  { value: 'all', label: t('tasks.overview.filterAll') },
  { value: 'running', label: t('tasks.overview.filterRunning') },
]);

// -------------------------------------------------------------------------
// Load — 2s polling, paused while the tab is hidden; the monotonic token
// discards responses from a superseded load. Re-runs when the backend
// switches so the v3 gate re-evaluates.
// -------------------------------------------------------------------------
let loadSeq = 0;
let pollTimer: ReturnType<typeof setInterval> | null = null;

async function load(opts: { silent?: boolean } = {}): Promise<void> {
  const seq = ++loadSeq;
  error.value = '';
  if (!isV3.value) {
    tasks.value = [];
    loading.value = false;
    return;
  }
  if (!opts.silent) loading.value = true;
  try {
    const fresh = await api.listWorkspaceTasks({ activeOnly: false });
    if (seq !== loadSeq) return;
    tasks.value = fresh;
  } catch (e) {
    if (seq === loadSeq) error.value = e instanceof Error ? e.message : String(e);
  } finally {
    if (seq === loadSeq) loading.value = false;
  }
}

function startPolling(): void {
  stopPolling();
  void load();
  pollTimer = setInterval(() => {
    if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return;
    void load({ silent: true });
  }, POLL_INTERVAL_MS);
}

function stopPolling(): void {
  if (pollTimer !== null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

watch(backend, () => { void load(); }, { immediate: true });
watch(isV3, (v3) => { if (v3) startPolling(); else stopPolling(); }, { immediate: true });
onBeforeUnmount(stopPolling);

// The elapsed-timer clock — one shared 1s tick, only while the panel shows
// at least one running row.
let clockTimer: ReturnType<typeof setInterval> | null = null;
watch(
  () => counts.value.running > 0,
  (hasRunning) => {
    if (hasRunning && clockTimer === null) {
      clockTimer = setInterval(() => {
        taskClock.value = (taskClock.value + 1) % Number.MAX_SAFE_INTEGER;
      }, 1000);
    } else if (!hasRunning && clockTimer !== null) {
      clearInterval(clockTimer);
      clockTimer = null;
    }
  },
  { immediate: true },
);
onBeforeUnmount(() => {
  if (clockTimer !== null) clearInterval(clockTimer);
});

// -------------------------------------------------------------------------
// Row presentation
// -------------------------------------------------------------------------
function statusBadgeVariant(status: AppTaskStatus): 'success' | 'danger' | 'warning' | 'neutral' {
  if (status === 'running') return 'warning';
  if (status === 'completed') return 'success';
  if (status === 'failed') return 'danger';
  return 'neutral';
}

function statusLabel(status: AppTaskStatus): string {
  if (status === 'completed') return t('tasks.notice.statusCompleted');
  if (status === 'failed') return t('tasks.notice.statusFailed');
  if (status === 'cancelled') return t('tasks.notice.statusKilled');
  return t('tasks.running');
}

function sessionLabel(group: { sessionId: string; sessionTitle: string }): string {
  // Unknown session (not in the local list) — the honest id placeholder,
  // never a fabricated title.
  return group.sessionTitle !== '' ? group.sessionTitle : group.sessionId;
}

function formatTime(iso: string | undefined): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '—';
  return new Intl.DateTimeFormat(locale.value, { dateStyle: 'medium', timeStyle: 'short' }).format(d);
}

/** Elapsed-since-registration for a running row; empty once terminal. */
function elapsed(iso: string | undefined): string {
  void taskClock.value;
  if (!iso) return '';
  const start = new Date(iso).getTime();
  if (Number.isNaN(start)) return '';
  const totalSec = Math.max(0, Math.floor((Date.now() - start) / 1000));
  if (totalSec < 60) return `${totalSec}s`;
  if (totalSec < 3600) return `${Math.floor(totalSec / 60)}m ${totalSec % 60}s`;
  return `${Math.floor(totalSec / 3600)}h ${Math.floor((totalSec % 3600) / 60)}m`;
}

// -------------------------------------------------------------------------
// Row actions
// -------------------------------------------------------------------------
async function onStop(row: WorkspaceTaskRow): Promise<void> {
  const ok = await confirm({
    title: t('tasks.overview.stopConfirmTitle'),
    message: t('tasks.overview.stopConfirmBody'),
    variant: 'danger',
  });
  if (!ok) return;
  stoppingId.value = row.task.id;
  try {
    await api.cancelTask(row.sessionId, row.task.id);
    // The 2s poll refreshes the row; a silent refresh confirms it now.
    await load({ silent: true });
  } catch (e) {
    client.pushOperationFailure('cancelTask', e);
  } finally {
    stoppingId.value = null;
  }
}
</script>

<template>
  <div class="tasks-panel">
    <!-- Header -->
    <div class="panel-head">
      <div class="panel-kicker">{{ t('settings.tabs.tasks') }}</div>
      <h4 class="panel-title">{{ t('tasks.overview.title') }}</h4>
      <p class="panel-desc">{{ t('tasks.overview.desc') }}</p>
    </div>

    <!-- Toolbar -->
    <div class="toolbar">
      <ToolbarSearch
        v-model="searchQuery"
        :placeholder="t('tasks.overview.search')"
        :aria-label="t('tasks.overview.search')"
      />
      <SegmentedControl v-model="statusFilter" :options="filterOptions" />
      <Button variant="secondary" size="sm" :disabled="!isV3 || loading" @click="load()">
        {{ t('tasks.overview.refresh') }}
      </Button>
    </div>

    <div v-if="error" class="error-msg">{{ error }}</div>

    <!-- v3-only honest degrade: no fake rows on the v1/v2 REST face -->
    <Banner v-if="!isV3" variant="info">{{ t('tasks.overview.v3Only') }}</Banner>

    <EmptyState v-else-if="loading && filtered.length === 0">
      <Spinner />
    </EmptyState>

    <EmptyState
      v-else-if="filtered.length === 0 && searchQuery.trim() !== ''"
      :title="t('tasks.overview.noMatch', { query: searchQuery.trim() })"
    />

    <EmptyState
      v-else-if="filtered.length === 0"
      :title="t('tasks.overview.empty')"
      :hint="t('tasks.overview.emptyHint')"
    >
      <template #icon><Icon name="clock" /></template>
    </EmptyState>

    <!-- Counters -->
    <div v-if="filtered.length > 0" class="counters">
      <Badge variant="warning" size="sm">{{ t('tasks.overview.runningCount', { n: counts.running }) }}</Badge>
      <Badge variant="success" size="sm">{{ t('tasks.overview.completedCount', { n: counts.completed }) }}</Badge>
      <Badge variant="danger" size="sm">{{ t('tasks.overview.failedCount', { n: counts.failed }) }}</Badge>
      <Badge v-if="counts.other > 0" variant="neutral" size="sm">{{ t('tasks.overview.otherCount', { n: counts.other }) }}</Badge>
    </div>

    <!-- Rows grouped by session -->
    <div v-if="filtered.length > 0" class="groups">
      <section v-for="group in grouped" :key="group.sessionId" class="group">
        <header class="group-head">
          <span class="group-title" :title="sessionLabel(group)">{{ sessionLabel(group) }}</span>
          <Badge v-if="group.archived" variant="neutral" size="sm">{{ t('tasks.overview.archivedSession') }}</Badge>
        </header>
        <div class="task-rows">
          <div v-for="row in group.rows" :key="row.task.id" class="task-row">
            <Badge :variant="statusBadgeVariant(row.task.status)" size="sm">{{ statusLabel(row.task.status) }}</Badge>
            <div class="task-main">
              <span class="task-desc" :title="row.task.description">{{ row.task.description }}</span>
              <span class="task-meta">
                {{ t('tasks.overview.started') }}: {{ formatTime(row.task.createdAt) }}
                <template v-if="row.task.status === 'running' && elapsed(row.task.createdAt) !== ''">
                  · {{ t('tasks.overview.elapsed', { time: elapsed(row.task.createdAt) }) }}
                </template>
              </span>
            </div>
            <Button
              v-if="row.task.status === 'running'"
              variant="danger-soft"
              size="sm"
              :disabled="stoppingId === row.task.id"
              @click="onStop(row)"
            >
              <Spinner v-if="stoppingId === row.task.id" size="sm" />
              {{ t('tasks.overview.stop') }}
            </Button>
          </div>
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
.tasks-panel { display: flex; flex-direction: column; height: 100%; gap: var(--space-3); overflow-y: auto; }

.panel-head { margin-bottom: var(--space-2); }
.panel-kicker { font-size: var(--text-xs); letter-spacing: 0.08em; text-transform: uppercase; color: var(--color-text-faint); margin-bottom: var(--space-1); }
.panel-title { margin: 0 0 var(--space-2); font-family: var(--font-ui); font-size: var(--text-2xl); font-weight: var(--weight-semibold); letter-spacing: -0.01em; color: var(--color-text); }
.panel-desc { margin: 0; font-family: var(--font-ui); font-size: var(--text-sm); line-height: var(--leading-normal); color: var(--color-text-muted); max-width: 560px; }

.toolbar { display: flex; align-items: center; gap: var(--space-3); flex-wrap: wrap; }

.error-msg { padding: var(--space-2) var(--space-3); border-radius: var(--radius-md); background: var(--color-danger-soft); color: var(--color-danger); font-size: var(--text-sm); }

.counters { display: flex; gap: 4px; flex-wrap: wrap; }

.groups { display: flex; flex-direction: column; gap: var(--space-3); min-height: 0; }

.group {
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  overflow: hidden;
}

.group-head {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  background: var(--color-surface-sunken);
  border-bottom: 1px solid var(--color-line);
}
.group-title {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text);
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.task-rows { display: flex; flex-direction: column; }

.task-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-2) var(--space-3);
}

.task-row + .task-row { border-top: 1px solid var(--color-line); }

.task-main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.task-desc {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.task-meta {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: var(--color-text-faint);
}

@media (max-width: 640px) {
  .toolbar { flex-wrap: wrap; }
  .task-row { flex-wrap: wrap; }
}
</style>
