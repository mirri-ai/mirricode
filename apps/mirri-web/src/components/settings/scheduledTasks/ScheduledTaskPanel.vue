<!-- apps/mirri-web/src/components/settings/scheduledTasks/ScheduledTaskPanel.vue -->
<!-- The settings page's scheduled-task tab (cron-improvement A 组). It has no
     business with any session: the page answers ONE question — "which
     routine work did I delegate" — and the list is the ALL-WORKSPACE
     aggregate (A14): every workspace's tasks, grouped by workspace with the
     active one first.

     Three views swap inside the content area (A11 — never a dialog or a
     drawer):
     - list: every workspace's task groups (a heading + a two-column card
       grid per group, §5), a status filter tab row, and the empty-state
       guidance (A10);
     - form: the single-column creation form (§5);
     - detail: one task's 设置 / 历史 tabs (§5).
     The create entry has NO precondition beyond the v3 engine (A1). An
     agent-raised creation is decided on the chat approval card only
     (ADR-022: one approval, no 待确认 section). -->
<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppCronTask, AppModel, AppWorkspace } from '../../../api/types';
import { getMirriWebApi } from '../../../api';
import { useMirriWebClient } from '../../../composables/useMirriWebClient';
import {
  cronDraftToInput,
  filterScheduledTasks,
  groupTasksByWorkspace,
  newCronDraft,
  scheduledTaskFilterCounts,
  type CronCreateDraft,
  type GroupedCronTasks,
  type ScheduledTaskFilter,
  type ScheduledTaskRunFacts,
} from '../../../lib/cronManagement';
import Banner from '../../ui/Banner.vue';
import Button from '../../ui/Button.vue';
import EmptyState from '../../ui/EmptyState.vue';
import Spinner from '../../ui/Spinner.vue';
import ScheduledTaskCard from './ScheduledTaskCard.vue';
import ScheduledTaskDetail from './ScheduledTaskDetail.vue';
import ScheduledTaskForm from './ScheduledTaskForm.vue';

const props = defineProps<{
  /** The catalog models the form's model picker lists. */
  models?: AppModel[];
}>();

const { t } = useI18n();
const api = getMirriWebApi();
const client = useMirriWebClient();
const { backend, activeWorkspaceId } = client;

const isV3 = computed(() => backend.value === 'v3');

// -------------------------------------------------------------------------
// View state
// -------------------------------------------------------------------------
type View = 'list' | 'form' | 'detail';
const view = ref<View>('list');
const filter = ref<ScheduledTaskFilter>('all');
/** The task the detail view shows (by id — the row is re-read from the list
 *  on every refresh so an edit/pause lands immediately). */
const selectedId = ref<string | null>(null);

/** A14: every workspace's tasks, grouped — the panel reads the whole
 *  aggregate, the active workspace's group sorts first, then by workspace
 *  name. A workspace with no tasks produces no group. The template consumes
 *  the groups (the grouped markup lands with the template batch); the data
 *  layer lands first. */
const allTasks = ref<AppCronTask[]>([]);
const taskGroups = computed<GroupedCronTasks[]>(() =>
  groupTasksByWorkspace(allTasks.value, workspaceRows.value, workspaceId.value),
);
const workspaces = ref<AppWorkspace[]>([]);
const schedulingDisabled = ref(false);
const loading = ref(false);
const error = ref('');
const saving = ref(false);
/** Bumped after every mutation so the detail's history reloads. */
const refreshKey = ref(0);
/** The shared clock the cards' relative labels read. */
const now = ref(Date.now());
let tick: ReturnType<typeof setInterval> | null = null;

/** The workspace the page addresses: the app's current workspace, else the
 *  engine default ('' lets the RPC resolve it). */
const workspaceId = computed(() => activeWorkspaceId.value ?? '');

const workspaceRows = computed(() =>
  workspaces.value.map((workspace) => ({ id: workspace.id, name: workspace.name || workspace.id })),
);

const selectedTask = computed(
  () => allTasks.value.find((task) => task.id === selectedId.value) ?? null,
);

/**
 * The failed tab's verdicts. The list row carries the honest projections the
 * engine already sends: a recurring task whose only recorded activity is a
 * past fire and which has NO upcoming fire has stopped scheduling (the
 * budget is spent or the engine gave up) — that is the "it stopped working"
 * signal the tab surfaces. The per-run detail stays in the detail's history.
 */
const runFacts = computed<Record<string, ScheduledTaskRunFacts>>(() => {
  const map: Record<string, ScheduledTaskRunFacts> = {};
  for (const task of allTasks.value) {
    const stopped =
      task.recurring === true &&
      task.lastFiredAt !== undefined &&
      task.lastFiredAt !== null &&
      task.nextFireAt === null;
    if (stopped) map[task.id] = { lastRunFailed: true };
  }
  return map;
});

const visibleTasks = computed(() => filterScheduledTasks(allTasks.value, filter.value, runFacts.value));

/** A14: the existing filter applied INSIDE one group — the filter tab stays
 *  cross-group (the counts read the whole aggregate), the grid shows a
 *  group's survivors. */
function visibleTasksOf(group: GroupedCronTasks): AppCronTask[] {
  return filterScheduledTasks(group.tasks, filter.value, runFacts.value);
}

const counts = computed(() => scheduledTaskFilterCounts(allTasks.value, runFacts.value));

const filterTabs = computed<Array<{ id: ScheduledTaskFilter; label: string; count: number }>>(() => [
  { id: 'all', label: t('settings.scheduledTaskFilterAll'), count: counts.value.all },
  { id: 'active', label: t('settings.scheduledTaskFilterActive'), count: counts.value.active },
  { id: 'paused', label: t('settings.scheduledTaskFilterPaused'), count: counts.value.paused },
  { id: 'failed', label: t('settings.scheduledTaskFilterFailed'), count: counts.value.failed },
]);

// -------------------------------------------------------------------------
// Load
// -------------------------------------------------------------------------
let loadSeq = 0;

async function load(opts: { silent?: boolean } = {}): Promise<void> {
  const seq = ++loadSeq;
  error.value = '';
  if (!isV3.value) {
    allTasks.value = [];
    loading.value = false;
    return;
  }
  if (!opts.silent) loading.value = true;
  try {
    const [aggregate, workspaceRowsList] = await Promise.all([
      api.listAllCronTasks(),
      api.listWorkspaces(),
    ]);
    if (seq !== loadSeq) return;
    schedulingDisabled.value = aggregate.disabled;
    workspaces.value = workspaceRowsList;
    // A14: keep EVERY workspace's tasks — the list groups them (the active
    // workspace first); the filter tabs and the detail view read the whole
    // aggregate.
    allTasks.value = (aggregate.workspaces ?? []).flatMap((group) => group.tasks);
  } catch (loadError) {
    if (seq === loadSeq) error.value = loadError instanceof Error ? loadError.message : String(loadError);
  } finally {
    if (seq === loadSeq) loading.value = false;
  }
}

onMounted(() => {
  void load();
  // The relative next-run labels' freshness.
  tick = setInterval(() => {
    now.value = Date.now();
  }, 30_000);
});
onBeforeUnmount(() => {
  if (tick !== null) clearInterval(tick);
});

// -------------------------------------------------------------------------
// Creation (A1: the entry always works; the workspace comes from the list)
// -------------------------------------------------------------------------
const initialDraft = ref<CronCreateDraft>(newCronDraft(''));

function openCreate(): void {
  const target =
    workspaceId.value !== ''
      ? workspaceId.value
      : (workspaces.value[0]?.id ?? '');
  initialDraft.value = newCronDraft(target);
  view.value = 'form';
}

function draftWorkspaceOf(draft: CronCreateDraft): string {
  return draft.workspaceId || workspaceId.value || workspaces.value[0]?.id || '';
}

async function onCreate(draft: CronCreateDraft, previewT0: number | null): Promise<void> {
  const target = draftWorkspaceOf(draft);
  if (target === '') {
    client.pushOperationFailure('createScheduledTask', new Error('no workspace to create in'), {
      title: t('settings.scheduledTaskLoadError'),
      message: t('settings.scheduledTaskV3Only'),
    });
    return;
  }
  saving.value = true;
  try {
    const created = await api.createWorkspaceCronTask(target, cronDraftToInput(draft, undefined, previewT0));
    view.value = 'list';
    selectedId.value = created.id;
    await load({ silent: true });
  } catch (error) {
    client.pushOperationFailure('createScheduledTask', error);
  } finally {
    saving.value = false;
  }
}

// -------------------------------------------------------------------------
// Detail actions (A9)
// -------------------------------------------------------------------------
function openDetail(task: AppCronTask): void {
  selectedId.value = task.id;
  view.value = 'detail';
}

async function onSave(draft: CronCreateDraft, previewT0: number | null): Promise<void> {
  const task = selectedTask.value;
  if (task === null) return;
  const owner = taskWorkspace(task);
  const input = cronDraftToInput(draft, undefined, previewT0);
  saving.value = true;
  try {
    // A9: the workspace is never in the payload — ownership does not move.
    await api.updateWorkspaceCronTask(owner, task.id, {
      title: draft.title.trim() === '' ? null : draft.title.trim(),
      prompt: input.prompt,
      cron: input.interval ? undefined : input.cron,
      interval: input.interval ?? null,
      timezone: input.timezone ?? null,
      endsAt: input.endsAt ?? null,
      maxFires: input.maxFires ?? null,
      model: input.model ?? null,
      effort: input.effort ?? null,
      permissionMode: input.permissionMode,
    });
    refreshKey.value++;
    await load({ silent: true });
  } catch (error) {
    client.pushOperationFailure('updateScheduledTask', error);
  } finally {
    saving.value = false;
  }
}

async function onTogglePause(): Promise<void> {
  const task = selectedTask.value;
  if (task === null) return;
  const owner = taskWorkspace(task);
  saving.value = true;
  try {
    if (task.paused === true) {
      await api.resumeWorkspaceCronTask(owner, task.id);
    } else {
      await api.pauseWorkspaceCronTask(owner, task.id);
    }
    refreshKey.value++;
    await load({ silent: true });
  } catch (error) {
    client.pushOperationFailure('togglePauseScheduledTask', error);
  } finally {
    saving.value = false;
  }
}

async function onDelete(): Promise<void> {
  const task = selectedTask.value;
  if (task === null) return;
  const owner = taskWorkspace(task);
  saving.value = true;
  try {
    await api.deleteWorkspaceCronTask(owner, task.id);
    selectedId.value = null;
    view.value = 'list';
    await load({ silent: true });
  } catch (error) {
    client.pushOperationFailure('deleteScheduledTask', error);
  } finally {
    saving.value = false;
  }
}

/** The workspace a task belongs to (the row's own scope; the page's as the
 *  fallback for a row without one). */
function taskWorkspace(task: AppCronTask): string {
  return task.scope?.kind === 'workspace' ? task.scope.workspaceId : workspaceId.value;
}
</script>

<template>
  <div class="stp">
    <!-- Honest v3-only gate (A1: the only precondition). -->
    <Banner v-if="!isV3" variant="info">{{ t('settings.scheduledTaskV3Only') }}</Banner>

    <template v-else>
      <div v-if="error" class="stp-error">{{ error }}</div>
      <Banner v-if="schedulingDisabled && view === 'list'" variant="warning">
        {{ t('settings.scheduledTaskDisabledBanner') }}
      </Banner>

      <!-- ===================== LIST ===================== -->
      <template v-if="view === 'list'">
        <div class="stp-head">
          <div class="stp-head-text">
            <h3 class="stp-title">{{ t('settings.scheduledTaskTitle') }}</h3>
            <p class="stp-desc">{{ t('settings.scheduledTaskDesc') }}</p>
          </div>
          <Button variant="primary" size="sm" @click="openCreate">
            {{ t('settings.scheduledTaskCreate') }}
          </Button>
        </div>

        <!-- §5: status is a filter tab row, never a card badge. -->
        <div class="stp-filters" role="tablist">
          <button
            v-for="tab in filterTabs"
            :key="tab.id"
            type="button"
            role="tab"
            class="stp-filter"
            :class="{ 'is-on': filter === tab.id }"
            :aria-selected="filter === tab.id"
            @click="filter = tab.id"
          >
            {{ tab.label }}
            <span v-if="tab.count > 0" class="stp-filter-count">{{ tab.count }}</span>
          </button>
        </div>

        <EmptyState v-if="loading && allTasks.length === 0">
          <Spinner />
        </EmptyState>

        <!-- A10: the empty state carries the value line + the guided button. -->
        <EmptyState
          v-else-if="allTasks.length === 0"
          :title="t('settings.scheduledTaskEmptyAllTitle')"
          :hint="t('settings.scheduledTaskEmptyAllHint')"
        >
          <Button variant="primary" size="sm" @click="openCreate">
            {{ t('settings.scheduledTaskEmptyAction') }}
          </Button>
        </EmptyState>

        <EmptyState
          v-else-if="visibleTasks.length === 0"
          :title="t('settings.scheduledTaskFilterEmpty')"
        />

        <!-- A14: every workspace's tasks — one group heading + the two-column
             grid per group; the active workspace's group sorts first. -->
        <template v-else>
          <section
            v-for="group in taskGroups"
            :key="group.workspaceId"
            class="stp-group"
            :aria-label="t('settings.scheduledTaskGroupHeading', { name: group.workspaceName })"
          >
            <h4 class="stp-group-title">{{ t('settings.scheduledTaskGroupHeading', { name: group.workspaceName }) }}</h4>
            <!-- §5: two cards per row, zero action buttons on a card. -->
            <div class="stp-grid">
              <ScheduledTaskCard
                v-for="task in visibleTasksOf(group)"
                :key="task.id"
                :task="task"
                :now="now"
                @open="openDetail(task)"
              />
            </div>
            <p v-if="visibleTasksOf(group).length === 0" class="stp-filter-empty">
              {{ t('settings.scheduledTaskFilterEmpty') }}
            </p>
          </section>
        </template>
      </template>

      <!-- ===================== CREATE FORM ===================== -->
      <template v-else-if="view === 'form'">
        <div class="stp-crumbs">
          <button type="button" class="stp-crumb" @click="view = 'list'">
            {{ t('settings.scheduledTaskTitle') }}
          </button>
          <span class="stp-crumb-sep" aria-hidden="true">/</span>
          <span class="stp-crumb-current">{{ t('settings.scheduledTaskFormCreateTitle') }}</span>
        </div>
        <div class="stp-form-head">
          <div class="stp-form-head-text">
            <h3 class="stp-title">{{ t('settings.scheduledTaskFormCreateTitle') }}</h3>
            <p class="stp-desc">{{ t('settings.scheduledTaskFormCreateSubtitle') }}</p>
          </div>
        </div>
        <ScheduledTaskForm
          :key="`create:${initialDraft.workspaceId}`"
          :initial-draft="initialDraft"
          mode="create"
          :workspaces="workspaceRows"
          :models="props.models ?? []"
          :submitting="saving"
          @submit="onCreate"
          @cancel="view = 'list'"
        />
      </template>

      <!-- ===================== DETAIL ===================== -->
      <ScheduledTaskDetail
        v-else-if="selectedTask !== null"
        :task="selectedTask"
        :workspace-id="taskWorkspace(selectedTask)"
        :workspaces="workspaceRows"
        :models="props.models ?? []"
        :saving="saving"
        :refresh-key="refreshKey"
        @back="view = 'list'"
        @save="onSave"
        @toggle-pause="onTogglePause"
        @delete="onDelete"
      />
      <!-- The selected task vanished (deleted elsewhere / filtered out). -->
      <EmptyState v-else :title="t('settings.scheduledTaskFilterEmpty')" />
    </template>
  </div>
</template>

<style scoped>
.stp { display: flex; flex-direction: column; gap: var(--space-4); min-height: 0; flex: 1; overflow-y: auto; }
.stp-error {
  padding: var(--space-2) var(--space-3);
  border-radius: var(--radius-md);
  background: var(--color-danger-soft);
  color: var(--color-danger);
  font-size: var(--text-sm);
}
.stp-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--space-4);
  padding-top: var(--space-2);
}
.stp-head-text { display: flex; flex-direction: column; gap: var(--space-1); min-width: 0; }
.stp-title {
  margin: 0;
  font-family: var(--font-ui);
  font-size: var(--text-2xl);
  font-weight: var(--weight-semibold);
  letter-spacing: -0.01em;
  color: var(--color-text);
}
.stp-desc { margin: 0; max-width: 560px; font-family: var(--font-ui); font-size: var(--text-sm); line-height: var(--leading-normal); color: var(--color-text-muted); }
.stp-filters { display: flex; flex-wrap: wrap; gap: var(--space-1); }
.stp-filter {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  border: none;
  border-radius: var(--radius-md);
  background: transparent;
  padding: var(--space-1) var(--space-3);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-text-muted);
  cursor: pointer;
}
.stp-filter:hover { color: var(--color-text); background: var(--color-surface-sunken); }
.stp-filter.is-on { color: var(--color-text); background: var(--color-accent-soft); }
.stp-filter:focus-visible { outline: none; box-shadow: var(--p-focus-ring); }
.stp-filter-count { font-size: var(--text-xs); color: var(--color-text-faint); }
/* A14: one group per workspace — the group heading follows the pending
   section's title precedent. */
.stp-group { display: flex; flex-direction: column; gap: var(--space-2); }
.stp-group-title { margin: 0; font-family: var(--font-ui); font-size: var(--text-base); font-weight: var(--weight-semibold); color: var(--color-text); }
.stp-filter-empty { margin: 0; font-family: var(--font-ui); font-size: var(--text-sm); color: var(--color-text-faint); }
/* §5 列表页基准: exactly two columns, generous gaps, low density. */
.stp-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--space-3);
}
@media (max-width: 900px) {
  .stp-grid { grid-template-columns: minmax(0, 1fr); }
}
.stp-crumbs { display: flex; align-items: center; gap: var(--space-1); padding-top: var(--space-2); font-size: var(--text-xs); color: var(--color-text-faint); }
.stp-crumb { border: none; background: transparent; padding: 0; color: var(--color-text-muted); font-family: var(--font-ui); font-size: var(--text-xs); cursor: pointer; }
.stp-crumb:hover { color: var(--color-text); }
.stp-form-head-text { display: flex; flex-direction: column; gap: var(--space-1); }
</style>
