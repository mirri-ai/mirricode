<!-- apps/mirri-web/src/components/settings/RetentionPanel.vue -->
<!-- Session-retention management panel (Task 11). Renders inside the
     Settings dialog's "retention" tab. Self-fetching (CronPanel mode): the
     rows come from the v3 rust engine's retention.* RPCs directly.
     Two views — "archive" (uncompressed archive) and "compressed" — with
     the closed sortable-column sets, the tiered batch-clean buttons
     (30/90/180/365), single delete with a confirm dialog, and the strategy
     hint (retention.getPolicy). List refresh rides a 30s poll while the
     panel is visible (paused when the tab is hidden) — a simple poll beats
     wiring a new event push for a low-frequency management page. -->
<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';

import { getMirriWebApi } from '../../api';
import type {
  AppRetentionEntry,
  AppRetentionStats,
  AppRetentionView,
  AppRetentionPolicy,
} from '../../api/types';
import { useConfirmDialog } from '../../composables/useConfirmDialog';
import { useMirriWebClient } from '../../composables/useMirriWebClient';
import {
  RETENTION_FILTER_NONE,
  RETENTION_TIER_DAYS,
  selectionBytes,
  selectionIntersect,
  toggleSelection,
  toggleVisibleAll,
  defaultSortForView,
  filterRetentionEntries,
  formatRetentionBytes,
  retentionWorkspaceOptions,
  sortableKeysForView,
  sortRetentionEntries,
  tierClickDecision,
  tierEstimate,
  toggleRetentionSort,
} from '../../lib/retentionManagement';
import type { RetentionFilter } from '../../lib/retentionManagement';
import type { RetentionSort, RetentionSortKey } from '../../lib/retentionManagement';
import Badge from '../ui/Badge.vue';
import Banner from '../ui/Banner.vue';
import Button from '../ui/Button.vue';
import EmptyState from '../ui/EmptyState.vue';
import Spinner from '../ui/Spinner.vue';
import Switch from '../ui/Switch.vue';
import Checkbox from '../ui/Checkbox.vue';
import Select from '../ui/Select.vue';
import Tabs from '../ui/Tabs.vue';
import ToolbarSearch from '../ui/ToolbarSearch.vue';
import Tooltip from '../ui/Tooltip.vue';

const { t, locale } = useI18n();
const { confirm } = useConfirmDialog();
const api = getMirriWebApi();
const client = useMirriWebClient();
const { backend } = client;

// The management page rides the v3 rust engine's retention.* RPCs; the
// v1/v2 REST daemon has no retention routes and gets an honest hint instead.
const isV3 = computed(() => backend.value === 'v3');

// -------------------------------------------------------------------------
// State — the two views share one load pipeline (stats + both lists); the
// sort state resets to the view default on every view switch (US3.2's
// "when I first enter" semantics).
// -------------------------------------------------------------------------
const view = ref<AppRetentionView>('archived');
const archivedRows = ref<AppRetentionEntry[]>([]);
const compressedRows = ref<AppRetentionEntry[]>([]);
const stats = ref<AppRetentionStats | null>(null);
const policy = ref<AppRetentionPolicy | null>(null);
const loading = ref(false);
const error = ref('');
/** True while a tiered batch runs — the re-entrancy gate (US3.4). */
const tierBusy = ref(false);
/** The tier outcome banner (success + per-failure lines), cleared on the
 *  next load. `goneIds` carries SDR-011's confirm-time exclusions — selected
 *  ids that left the archive list before the batch confirmed (deleted or
 *  restored elsewhere, or auto-compressed into the compressed view). */
const tierOutcome = ref<{
  deletedCount: number;
  reclaimedBytes: number;
  failures: Array<{ sessionId: string; error: string }>;
  goneIds?: string[];
} | null>(null);
/** The row with an in-flight compress-now ack. */
const compressingId = ref<string | null>(null);

const sort = ref<RetentionSort>(defaultSortForView('archived'));

const rows = computed(() =>
  view.value === 'compressed' ? compressedRows.value : archivedRows.value,
);
// The archive view's client-side filter (SDR-009 — the merged legacy page's
// search + workspace filter); the compressed view never filters (ADR-010).
const archiveFilter = ref<RetentionFilter>({ ...RETENTION_FILTER_NONE });
const archiveWorkspaceOptions = computed(() => retentionWorkspaceOptions(archivedRows.value));
const filteredRows = computed(() =>
  view.value === 'compressed'
    ? rows.value
    : filterRetentionEntries(rows.value, archiveFilter.value),
);
const sortedRows = computed(() => sortRetentionEntries(filteredRows.value, sort.value));
const sortableKeys = computed(() => sortableKeysForView(view.value));

watch(view, (next) => {
  sort.value = defaultSortForView(next);
  archiveFilter.value = { ...RETENTION_FILTER_NONE };
});

// -------------------------------------------------------------------------
// Load — 30s polling while visible (paused when the tab is hidden); the
// monotonic token discards responses from a superseded load.
// -------------------------------------------------------------------------
const POLL_INTERVAL_MS = 30_000;
let loadSeq = 0;
let pollTimer: ReturnType<typeof setInterval> | null = null;

async function load(opts: { silent?: boolean } = {}): Promise<void> {
  const seq = ++loadSeq;
  if (!isV3.value) {
    archivedRows.value = [];
    compressedRows.value = [];
    stats.value = null;
    policy.value = null;
    loading.value = false;
    return;
  }
  if (!opts.silent) loading.value = true;
  try {
    const [archived, compressed, statsResult, policyResult] = await Promise.all([
      api.listRetention('archived'),
      api.listRetention('compressed'),
      api.getRetentionStats(),
      api.getRetentionPolicy(),
    ]);
    if (seq !== loadSeq) return;
    archivedRows.value = archived;
    compressedRows.value = compressed;
    stats.value = statsResult;
    policy.value = policyResult;
    tierOutcome.value = null;
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

watch(
  isV3,
  (v3) => {
    if (v3) startPolling();
    else stopPolling();
  },
  { immediate: true },
);
onBeforeUnmount(stopPolling);

// -------------------------------------------------------------------------
// Presentation
// -------------------------------------------------------------------------
function formatTime(iso: string | null): string {
  // An empty string is the unreadable-state stamp (ADR-010) — same
  // placeholder as a missing one.
  if (iso === null || iso === '') return t('settings.retentionUnknownTime');
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return t('settings.retentionUnknownTime');
  return new Intl.DateTimeFormat(locale.value, { dateStyle: 'medium', timeStyle: 'short' }).format(
    d,
  );
}

function formatDays(days: number | null): string {
  return days === null
    ? t('settings.retentionUnknownDays')
    : t('settings.retentionDaysValue', { days });
}

function formatSaved(savedBytes: number | null): string {
  return savedBytes === null
    ? t('settings.retentionUnknownSaved')
    : formatRetentionBytes(savedBytes);
}

function setSort(key: RetentionSortKey): void {
  sort.value = toggleRetentionSort(sort.value, key, view.value);
}

function sortIndicator(key: RetentionSortKey): string {
  if (sort.value.key !== key) return '';
  return sort.value.direction === 'asc'
    ? t('settings.retentionSortAsc')
    : t('settings.retentionSortDesc');
}

// The strategy hint (US3.5) — one sentence, not part of the list data.
const policyHint = computed<string | null>(() => {
  if (policy.value === null) return null;
  if (!policy.value.enabled) return t('settings.retentionPolicyOff');
  return t('settings.retentionPolicyOn', { days: policy.value.archiveGraceDays });
});

// -------------------------------------------------------------------------
// Auto-compression settings card (SDR-002's in-page enable entry): the
// switch + grace-days + interval-hours editor over `config.set`. The form
// seeds from `retention.getPolicy` (the resolved policy — defaults
// included) so what the user sees is what the engine would run; saving
// writes the whole section in one atomic `config.set`.
// -------------------------------------------------------------------------
const autoEnabled = ref(false);
const graceDays = ref(30);
const intervalHours = ref(24);
const policySaving = ref(false);
const policySaved = ref(false);
let policySaveTimer: ReturnType<typeof setTimeout> | null = null;

watch(policy, (next) => {
  if (next === null || policySaving.value) return;
  autoEnabled.value = next.enabled;
  graceDays.value = next.archiveGraceDays;
  intervalHours.value = next.intervalHours;
});

watch([autoEnabled, graceDays, intervalHours], () => {
  policySaved.value = false;
});

async function savePolicy(): Promise<void> {
  // Clamp before sending — the same bounds the engine's sanitize would
  // enforce, so the wire value is never surprising.
  const days = Math.min(Math.max(Math.trunc(graceDays.value) || 1, 1), 3650);
  const hours = Math.min(Math.max(Math.trunc(intervalHours.value) || 1, 1), 720);
  graceDays.value = days;
  intervalHours.value = hours;
  policySaving.value = true;
  try {
    await api.setConfig({
      retention: { enabled: autoEnabled.value, archiveGraceDays: days, intervalHours: hours },
    });
    policySaved.value = true;
    if (policySaveTimer !== null) clearTimeout(policySaveTimer);
    policySaveTimer = setTimeout(() => (policySaved.value = false), 2000);
    // The engine hot-reloads the in-memory config on write; re-pull so the
    // hint reflects the resolved policy (not just what we sent).
    policy.value = await api.getRetentionPolicy();
  } catch (e) {
    client.pushOperationFailure('retentionSetPolicy', e);
  } finally {
    policySaving.value = false;
  }
}

onBeforeUnmount(() => {
  if (policySaveTimer !== null) clearTimeout(policySaveTimer);
});

// -------------------------------------------------------------------------
// Row actions
// -------------------------------------------------------------------------
async function onRestore(row: AppRetentionEntry): Promise<void> {
  // Restore rides the existing session.restore path (the same one the
  // archived tab uses) — the rust handler decompresses before clearing the
  // archive flag; the row then leaves this page on the next poll/refresh.
  // A failure already surfaced as a toast by the composable — skip the
  // reload so the row visibly stays until the user retries.
  const ok = await client.restoreSession(row.sessionId);
  if (ok) void load({ silent: true });
}

async function onCompressNow(row: AppRetentionEntry): Promise<void> {
  compressingId.value = row.sessionId;
  try {
    await api.compressRetentionSession(row.sessionId);
    await load({ silent: true });
  } catch (e) {
    client.pushOperationFailure('retentionCompressNow', e);
  } finally {
    compressingId.value = null;
  }
}

async function onDelete(row: AppRetentionEntry): Promise<void> {
  const ok = await confirm({
    title: t('settings.retentionDeleteTitle'),
    message: t('settings.retentionDeleteBody', {
      title: row.title,
      size: formatRetentionBytes(row.currentSizeBytes),
    }),
    confirmLabel: t('settings.retentionDeleteConfirm'),
    variant: 'danger',
  });
  if (!ok) return;
  try {
    await api.deleteRetentionSession(row.sessionId);
    await load({ silent: true });
  } catch (e) {
    client.pushOperationFailure('retentionDelete', e);
  }
}

// -------------------------------------------------------------------------
// Multi-select delete (SDR-011) — checkboxes on the archive view, one
// confirm over the confirm-time intersection, sequential single deletes,
// per-item accounting in the outcome banner.
// -------------------------------------------------------------------------
/** The archive view's checked session ids (SDR-011: keyed by id so the
 *  selection survives filter/sort/view switches and the poll refresh). */
const selectedIds = ref<Set<string>>(new Set());

function toggleRowSelection(id: string): void {
  selectedIds.value = new Set(toggleSelection({ ids: selectedIds.value }, id).ids);
}

/** The visible rows' checkbox states (drives the header's select-all). */
const visibleIds = computed(() => sortedRows.value.map((row) => row.sessionId));
const allVisibleSelected = computed(
  () => visibleIds.value.length > 0 && visibleIds.value.every((id) => selectedIds.value.has(id)),
);
const someVisibleSelected = computed(
  () => visibleIds.value.some((id) => selectedIds.value.has(id)),
);

function onToggleAllVisible(): void {
  selectedIds.value = new Set(toggleVisibleAll({ ids: selectedIds.value }, visibleIds.value).ids);
}

async function onDeleteSelected(): Promise<void> {
  // The confirm-time intersection against the UNFILTERED archive list
  // (US3.3a: filter-hidden selected ids still count).
  const { rows: kept, goneIds } = selectionIntersect(
    { ids: selectedIds.value },
    archivedRows.value,
  );
  // All-gone: no confirm, no delete — a gone-only outcome banner.
  if (kept.length === 0) {
    tierOutcome.value = { deletedCount: 0, reclaimedBytes: 0, failures: [], goneIds };
    selectedIds.value = new Set();
    return;
  }
  const bytes = selectionBytes(kept);
  const ok = await confirm({
    title: t('settings.retentionDeleteSelectedTitle'),
    message: t('settings.retentionTierConfirmBody', {
      count: kept.length,
      size: formatRetentionBytes(bytes),
    }),
    confirmLabel: t('settings.retentionDeleteConfirm'),
    variant: 'danger',
    action: async () => {
      tierBusy.value = true;
      try {
        let deletedCount = 0;
        let reclaimedBytes = 0;
        const failures: Array<{ sessionId: string; error: string }> = [];
        // Sequential: one delete at a time (the same executor shape as the
        // rust tiered batch) — no directory-lock contention amplification.
        for (const row of kept) {
          try {
            const result = await api.deleteRetentionSession(row.sessionId);
            deletedCount += 1;
            reclaimedBytes += result.reclaimedBytes;
          } catch (e) {
            failures.push({ sessionId: row.sessionId, error: String(e) });
          }
        }
        await load({ silent: true });
        tierOutcome.value = { deletedCount, reclaimedBytes, failures, goneIds };
        selectedIds.value = new Set();
      } finally {
        tierBusy.value = false;
      }
    },
  });
  void ok;
}

// -------------------------------------------------------------------------
// Tiered batch clean (US3.4) — estimate from stats, confirm with count +
// bytes + irreversibility, re-entrancy gate, honest partial accounting.
// -------------------------------------------------------------------------
function onTierClick(days: number): void {
  // The pure decision (US3.4): confirm with count/bytes math on a non-empty
  // scope, the empty hint otherwise — and nothing at all while a batch runs.
  const decision = tierClickDecision(stats.value, days, tierBusy.value);
  if (decision.kind === 'blocked') return;
  if (decision.kind === 'empty') {
    void confirm({
      title: t('settings.retentionTierEmptyTitle'),
      message: t('settings.retentionTierEmptyBody', { days }),
      variant: 'primary',
    });
    return;
  }
  void confirmTiered(decision.days, decision.count, decision.bytes);
}

async function confirmTiered(days: number, count: number, bytes: number): Promise<void> {
  const ok = await confirm({
    title: t('settings.retentionTierConfirmTitle', { days }),
    message: t('settings.retentionTierConfirmBody', {
      count,
      size: formatRetentionBytes(bytes),
    }),
    confirmLabel: t('settings.retentionDeleteConfirm'),
    variant: 'danger',
    action: async () => {
      tierBusy.value = true;
      try {
        const result = await api.deleteRetentionTiered(days);
        await load({ silent: true });
        tierOutcome.value = {
          deletedCount: result.deleted.length,
          reclaimedBytes: result.reclaimedBytes,
          failures: result.failed,
        };
      } catch (e) {
        // Same posture as the single-row delete: a network/backend failure
        // must surface as a toast, not vanish behind the confirm dialog.
        client.pushOperationFailure('retentionDeleteTiered', e);
      } finally {
        tierBusy.value = false;
      }
    },
  });
  void ok;
}
</script>

<template>
  <div class="retention-panel">
    <!-- Header -->
    <div class="panel-head">
      <div class="panel-kicker">{{ t('settings.tabs.retention') }}</div>
      <h4 class="panel-title">{{ t('settings.retentionTitle') }}</h4>
      <p class="panel-desc">{{ t('settings.retentionDesc') }}</p>
    </div>

    <!-- Strategy hint (US3.5): one sentence, not list data. -->
    <Banner v-if="policyHint !== null && !isV3" variant="info">{{ policyHint }}</Banner>

    <!-- Auto-compression settings card (SDR-002's in-page enable entry):
         the switch + grace-days + interval editor over config.set, so the
         user never has to hand-edit config.toml to turn the feature on. -->
    <div v-if="isV3" class="retention-policy-card">
      <div class="retention-policy-main">
        <div class="retention-policy-copy">
          <div class="retention-policy-title">{{ t('settings.retentionAutoTitle') }}</div>
          <div class="retention-policy-sub">
            {{ t('settings.retentionAutoDesc') }}
          </div>
        </div>
        <Switch
          :model-value="autoEnabled"
          :disabled="policySaving"
          @update:model-value="autoEnabled = $event"
        />
      </div>
      <div v-if="autoEnabled" class="retention-policy-fields">
        <label class="retention-policy-field">
          <span>{{ t('settings.retentionAutoGraceDays') }}</span>
          <input
            v-model.number="graceDays"
            type="number"
            min="1"
            max="3650"
            :disabled="policySaving"
          />
        </label>
        <label class="retention-policy-field">
          <span>{{ t('settings.retentionAutoInterval') }}</span>
          <input
            v-model.number="intervalHours"
            type="number"
            min="1"
            max="720"
            :disabled="policySaving"
          />
        </label>
      </div>
      <div class="retention-policy-actions">
        <Button
          variant="primary"
          size="sm"
          :disabled="policySaving || policy === null"
          @click="savePolicy"
        >
          {{ policySaving ? t('settings.retentionAutoSaving') : t('settings.retentionAutoSave') }}
        </Button>
        <span v-if="policySaved" class="retention-policy-saved">{{
          t('settings.retentionAutoSaved')
        }}</span>
      </div>
    </div>

    <!-- View switch -->
    <Tabs
      :model-value="view"
      :options="[
        { value: 'archived', label: t('settings.retentionViewArchived') },
        { value: 'compressed', label: t('settings.retentionViewCompressed') },
      ]"
      @update:model-value="view = $event as AppRetentionView"
    />

    <!-- Archive-view toolbar (SDR-009): title search + workspace filter —
         the merged legacy archived page's capabilities, client-side over
         the loaded rows. The compressed view never filters (ADR-010). -->
    <div v-if="view === 'archived' && isV3" class="retention-toolbar">
      <ToolbarSearch
        v-model="archiveFilter.query"
        class="retention-search"
        :placeholder="t('settings.retentionSearch')"
        :aria-label="t('settings.retentionSearch')"
      />
      <Select
        :model-value="archiveFilter.workspace"
        size="sm"
        :aria-label="t('settings.retentionAllWorkspaces')"
        @update:model-value="archiveFilter.workspace = $event as string"
      >
        <option value="all">{{ t('settings.retentionAllWorkspaces') }}</option>
        <option v-for="ws in archiveWorkspaceOptions" :key="ws" :value="ws">{{ ws }}</option>
      </Select>
      <Button
        v-if="selectedIds.size > 0"
        variant="danger-soft"
        size="sm"
        :disabled="tierBusy"
        @click="onDeleteSelected"
      >
        {{ t('settings.retentionDeleteSelected', { count: selectedIds.size }) }}
      </Button>
    </div>

    <div v-if="error" class="error-row">
      <span class="error-msg">{{ error }}</span>
      <Button variant="secondary" size="sm" @click="load()">{{
        t('settings.retentionRetry')
      }}</Button>
    </div>

    <!-- v3-only honest degrade: no fake rows on the v1/v2 REST face -->
    <Banner v-if="!isV3" variant="info">{{ t('settings.retentionV3Only') }}</Banner>

    <EmptyState v-else-if="loading && rows.length === 0">
      <Spinner />
    </EmptyState>

    <template v-else>
      <!-- Tiered batch clean (the compressed view's bulk release, US3.4). -->
      <div v-if="view === 'compressed'" class="tier-bar">
        <span class="tier-label">{{ t('settings.retentionTierLabel') }}</span>
        <Tooltip
          v-for="days in RETENTION_TIER_DAYS"
          :key="days"
          :text="
            tierEstimate(stats, days) !== null
              ? t('settings.retentionTierEstimate', {
                  count: tierEstimate(stats, days)!.count,
                  size: formatRetentionBytes(tierEstimate(stats, days)!.bytes),
                })
              : t('settings.retentionTierEstimateUnknown')
          "
        >
          <Button variant="secondary" size="sm" :disabled="tierBusy" @click="onTierClick(days)">
            {{ t('settings.retentionTierButton', { days }) }}
          </Button>
        </Tooltip>
      </div>

      <!-- Batch outcome (partial-failure accounting, US3.4). -->
      <Banner
        v-if="
          tierOutcome !== null &&
          (tierOutcome.deletedCount > 0 ||
            tierOutcome.failures.length > 0 ||
            (tierOutcome.goneIds?.length ?? 0) > 0)
        "
        :variant="tierOutcome.failures.length > 0 ? 'warning' : 'info'"
      >
        <div class="tier-outcome">
          <span
            v-if="tierOutcome.deletedCount > 0 || tierOutcome.failures.length > 0"
          >{{
            t('settings.retentionTierDone', {
              count: tierOutcome.deletedCount,
              size: formatRetentionBytes(tierOutcome.reclaimedBytes),
            })
          }}</span>
          <span
            v-if="(tierOutcome.goneIds?.length ?? 0) > 0"
            class="tier-gone"
          >
            {{ t('settings.retentionSelectedGone', { count: tierOutcome.goneIds?.length ?? 0 }) }}
          </span>
          <span
            v-for="failure in tierOutcome.failures"
            :key="failure.sessionId"
            class="tier-failure"
            >{{
              t('settings.retentionTierFailure', { id: failure.sessionId, reason: failure.error })
            }}</span
          >
        </div>
      </Banner>

      <!-- Empty state (US3.2): a message, never a blank frame. The archive
           view distinguishes "no archived sessions" from "no match" (the
           SDR-009 filter's no-hit state). -->
      <div v-if="sortedRows.length === 0" class="retention-empty">
        {{
          view === 'compressed'
            ? t('settings.retentionCompressedEmpty')
            : filteredRows.length === 0 && archivedRows.length > 0
              ? t('settings.retentionNoMatch')
              : t('settings.retentionArchivedEmpty')
        }}
      </div>

      <!-- Compressed view column layout (one extra column set via a class). -->
      <div
        v-if="sortedRows.length > 0"
        class="setting-card retention-table"
        :class="{ 'compressed-cols': view === 'compressed' }"
        role="table"
      >
        <!-- The head rides the same view-scoped column template as the rows
             (the compressed view's 7th column: Saved) — without the class
             the header grid stays at 6 columns and misaligns. -->
        <div
          class="retention-head"
          :class="{ 'compressed-cols': view === 'compressed' }"
          role="row"
        >
          <span v-if="view === 'archived'" class="col-check" role="columnheader">
            <Checkbox
              :model-value="allVisibleSelected"
              :indeterminate="someVisibleSelected && !allVisibleSelected"
              :disabled="tierBusy"
              :aria-label="t('settings.retentionSelectAll')"
              @update:model-value="onToggleAllVisible()"
            />
          </span>
          <button
            v-if="view === 'archived' && sortableKeys.includes('title')"
            type="button"
            class="col-sort"
            role="columnheader"
            @click="setSort('title')"
          >
            {{ t('settings.retentionColTitle')
            }}<span class="dir">{{ sortIndicator('title') }}</span>
          </button>
          <span v-else class="col-title" role="columnheader">{{
            t('settings.retentionColTitle')
          }}</span>
          <span class="col-ws" role="columnheader">{{ t('settings.retentionColWorkspace') }}</span>
          <button
            v-if="view === 'compressed'"
            type="button"
            class="col-sort"
            role="columnheader"
            @click="setSort('archivedAt')"
          >
            {{ t('settings.retentionColCompressedAt')
            }}<span class="dir">{{ sortIndicator('archivedAt') }}</span>
          </button>
          <button
            v-else
            type="button"
            class="col-sort"
            role="columnheader"
            @click="setSort('archivedAt')"
          >
            {{ t('settings.retentionColArchivedAt')
            }}<span class="dir">{{ sortIndicator('archivedAt') }}</span>
          </button>
          <button
            v-if="view === 'archived'"
            type="button"
            class="col-sort"
            role="columnheader"
            @click="setSort('createdAt')"
          >
            {{ t('settings.retentionColCreatedAt')
            }}<span class="dir">{{ sortIndicator('createdAt') }}</span>
          </button>
          <button
            v-if="view === 'compressed'"
            type="button"
            class="col-sort"
            role="columnheader"
            @click="setSort('days')"
          >
            {{ t('settings.retentionColCompressedDays')
            }}<span class="dir">{{ sortIndicator('days') }}</span>
          </button>
          <button type="button" class="col-sort" role="columnheader" @click="setSort('size')">
            {{ t('settings.retentionColSize') }}<span class="dir">{{ sortIndicator('size') }}</span>
          </button>
          <span v-if="view === 'compressed'" class="col-saved" role="columnheader">
            {{ t('settings.retentionColSaved') }}
          </span>
          <span class="col-actions" role="columnheader">{{
            t('settings.retentionColActions')
          }}</span>
        </div>
        <div
          v-for="row in sortedRows"
          :key="row.sessionId"
          class="retention-row"
          :class="{
            'compressed-cols': view === 'compressed',
            busy: compressingId === row.sessionId,
          }"
          role="row"
        >
          <span v-if="view === 'archived'" class="col-check" role="cell">
            <Checkbox
              :model-value="selectedIds.has(row.sessionId)"
              :disabled="tierBusy"
              :aria-label="t('settings.retentionSelectRow', { title: row.title })"
              @update:model-value="toggleRowSelection(row.sessionId)"
            />
          </span>
          <span class="col-title cell-title" role="cell">
            {{ row.title }}
            <Badge
              v-if="view === 'archived' && row.compressedAt !== null"
              variant="neutral"
              size="sm"
            >
              {{ t('settings.retentionCompressedBadge') }}
            </Badge>
          </span>
          <span class="col-ws cell-ws" role="cell" :title="row.workspace">{{ row.workspace }}</span>
          <span v-if="view === 'archived'" class="cell-time" role="cell">{{
            formatTime(row.archivedAt)
          }}</span>
          <span v-if="view === 'archived'" class="cell-time" role="cell">{{
            formatTime(row.createdAt)
          }}</span>
          <span v-if="view === 'compressed'" class="cell-time" role="cell">{{
            formatTime(row.compressedAt)
          }}</span>
          <span v-if="view === 'compressed'" class="cell-days" role="cell">{{
            formatDays(row.compressedDays)
          }}</span>
          <span class="cell-size" role="cell">{{
            formatRetentionBytes(row.currentSizeBytes)
          }}</span>
          <span v-if="view === 'compressed'" class="cell-saved" role="cell">{{
            formatSaved(row.savedBytes)
          }}</span>
          <span class="col-actions cell-actions" role="cell">
            <Button variant="secondary" size="sm" @click="onRestore(row)">
              {{ t('settings.retentionRestore') }}
            </Button>
            <Button
              v-if="view === 'archived'"
              variant="secondary"
              size="sm"
              :disabled="compressingId === row.sessionId || row.compressedAt !== null"
              @click="onCompressNow(row)"
            >
              <Spinner v-if="compressingId === row.sessionId" size="sm" />
              {{ t('settings.retentionCompressNow') }}
            </Button>
            <Button variant="danger-soft" size="sm" @click="onDelete(row)">
              {{ t('settings.retentionDelete') }}
            </Button>
          </span>
        </div>
      </div>
    </template>
  </div>
</template>

<style scoped>
.retention-panel {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}

/* Auto-compression settings card: the enable entry (SDR-002) — a compact
   bordered card above the management list, switch-first so the off state
   still shows the feature exists. */
.retention-policy-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  padding: var(--space-3);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md, 8px);
}

.retention-policy-main {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
}

.retention-policy-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.retention-policy-title {
  font-weight: 600;
}

.retention-policy-sub {
  font-size: 12px;
  color: var(--text-secondary, inherit);
  opacity: 0.8;
}

.retention-policy-fields {
  display: flex;
  gap: var(--space-4);
  flex-wrap: wrap;
}

.retention-policy-field {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  font-size: 13px;
}

.retention-policy-field input {
  width: 5.5em;
  padding: 4px 8px;
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-sm, 6px);
  background: transparent;
  color: inherit;
}

.retention-policy-actions {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}

.retention-policy-saved {
  font-size: 12px;
  color: var(--success, #16a34a);
}

/* Archive-view toolbar (SDR-009): the merged legacy page's search + filter. */
.retention-toolbar {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  flex-wrap: wrap;
}

.retention-search {
  flex: 1 1 220px;
  min-width: 160px;
}

.panel-head {
  margin-bottom: var(--space-2);
}
.panel-kicker {
  font-size: var(--text-xs);
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--color-text-faint);
  margin-bottom: var(--space-1);
}
.panel-title {
  margin: 0 0 var(--space-2);
  font-family: var(--font-ui);
  font-size: var(--text-2xl);
  font-weight: var(--weight-semibold);
  letter-spacing: -0.01em;
  color: var(--color-text);
}
.panel-desc {
  margin: 0;
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  line-height: var(--leading-normal);
  color: var(--color-text-muted);
  max-width: 560px;
}

.error-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  flex-wrap: wrap;
}
.error-msg {
  padding: var(--space-2) var(--space-3);
  border-radius: var(--radius-md);
  background: var(--color-danger-soft);
  color: var(--color-danger);
  font-size: var(--text-sm);
  flex: 1 1 auto;
}

.tier-bar {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  flex-wrap: wrap;
}
.tier-label {
  font-size: var(--text-sm);
  color: var(--color-text-muted);
  flex: none;
}

.tier-outcome {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  align-items: flex-start;
}
.tier-failure {
  font-size: var(--text-xs);
}

.setting-card {
  border: 1px solid var(--color-line);
  border-radius: var(--radius-xl);
  overflow: hidden;
  background: var(--color-bg);
}
.retention-table {
  display: flex;
  flex-direction: column;
}

.retention-head,
.retention-row {
  display: grid;
  grid-template-columns:
    28px
    minmax(160px, 2fr)
    minmax(120px, 1.4fr)
    minmax(120px, 1fr)
    minmax(130px, 1fr)
    minmax(110px, 1fr)
    auto;
  gap: var(--space-2);
  align-items: center;
  padding: var(--space-2) var(--space-4);
}
.retention-head.compressed-cols,
.retention-row.compressed-cols {
  grid-template-columns:
    minmax(160px, 2fr)
    minmax(120px, 1.4fr)
    minmax(120px, 1fr)
    minmax(110px, 1fr)
    minmax(100px, 1fr)
    minmax(110px, 1fr)
    auto;
}

.retention-head {
  border-bottom: 1px solid var(--color-line);
  font-size: var(--text-xs);
  font-weight: var(--weight-medium);
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.04em;
}
.col-check {
  display: inline-flex;
  align-items: center;
}
.tier-gone {
  color: var(--color-text-muted);
}
.col-sort {
  border: none;
  background: transparent;
  padding: 0;
  font: inherit;
  color: inherit;
  text-transform: inherit;
  letter-spacing: inherit;
  text-align: left;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
}
.col-sort:hover {
  color: var(--color-text);
}
.col-sort:focus-visible {
  outline: none;
  box-shadow: var(--p-focus-ring);
  border-radius: var(--radius-sm);
}
.dir {
  font-size: var(--text-xs);
}

.retention-row {
  border-top: 1px solid var(--color-line);
  font-size: var(--text-sm);
}
.retention-row:first-of-type {
  border-top: none;
}
.retention-row:hover {
  background: var(--color-surface-sunken);
}

.cell-title {
  font-weight: var(--weight-medium);
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.cell-ws {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.cell-time,
.cell-days,
.cell-size,
.cell-saved {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  white-space: nowrap;
}
.cell-size {
  color: var(--color-text);
}
.cell-actions {
  display: inline-flex;
  gap: var(--space-2);
  justify-content: flex-end;
  flex-wrap: wrap;
}

.retention-empty {
  padding: var(--space-6) var(--space-4);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-xl);
  color: var(--color-text-faint);
  font-size: var(--text-sm);
  text-align: center;
  background: var(--color-bg);
}

@media (max-width: 720px) {
  .retention-head {
    display: none;
  }
  .retention-row {
    grid-template-columns: 28px 1fr auto;
    grid-template-areas:
      'check title actions'
      'check meta actions';
  }
  .col-check {
    grid-area: check;
  }
  .col-title {
    grid-area: title;
  }
  .col-ws,
  .cell-time,
  .cell-days,
  .cell-size,
  .col-saved {
    grid-area: meta;
  }
  .cell-actions {
    grid-area: actions;
  }
}
</style>
