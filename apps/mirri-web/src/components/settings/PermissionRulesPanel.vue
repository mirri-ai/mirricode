<!-- apps/mirri-web/src/components/settings/PermissionRulesPanel.vue -->
<!-- User-level permission rules management panel (B9 / D-34 / P-013).
     Renders inside the Settings dialog's "rules" tab. Self-fetching
     (CronPanel mode): rows come from the v3 rust engine's
     approval.listRules / addRule / removeRule RPCs directly — the
     persisted fact source is the mirri home config.toml's
     [[permission.rules]], effective for every session without a restart.
     Management is v3-only (backend-gated banner otherwise); the panel
     manages USER-scope rules only (project / turn-override /
     session-runtime scopes are not addressable through this face). -->
<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppPermissionRule, AppPermissionRuleDecision } from '../../api/types';
import { getMirriWebApi } from '../../api';
import { useMirriWebClient } from '../../composables/useMirriWebClient';
import { useConfirmDialog } from '../../composables/useConfirmDialog';
import {
  decisionBadgeVariant,
  groupRulesByDecision,
  ruleFacetPreview,
  validateDraft,
} from '../../lib/permissionRules';
import { parseRuleFacet } from '../../lib/ruleFacet';
import Badge from '../ui/Badge.vue';
import Banner from '../ui/Banner.vue';
import Button from '../ui/Button.vue';
import EmptyState from '../ui/EmptyState.vue';
import Icon from '../ui/Icon.vue';
import Input from '../ui/Input.vue';
import SegmentedControl from '../ui/SegmentedControl.vue';
import Spinner from '../ui/Spinner.vue';

const { t } = useI18n();
const { confirm } = useConfirmDialog();
const api = getMirriWebApi();
const client = useMirriWebClient();
const { backend } = client;

// -------------------------------------------------------------------------
// State
// -------------------------------------------------------------------------
const rules = ref<AppPermissionRule[]>([]);
const loading = ref(false);
const error = ref('');
/** The row whose detail pane is open (master-detail). */
const selectedKey = ref<string | null>(null);
/** The row with an in-flight remove ack. */
const removingKey = ref<string | null>(null);
/** The add form's in-flight ack. */
const adding = ref(false);
/** The add form draft (pattern + decision; reason optional). */
const draftDecision = ref<AppPermissionRuleDecision>('allow');
const draftPattern = ref('');
const draftReason = ref('');

// Rule management rides the v3 rust engine's approval.* RPCs (B9); the
// v1/v2 REST daemon has no rule routes and gets an honest hint instead.
const isV3 = computed(() => backend.value === 'v3');
const groups = computed(() => groupRulesByDecision(rules.value));
const groupedRows = computed<Array<{ decision: AppPermissionRuleDecision; rules: AppPermissionRule[] }>>(
  () => [
    { decision: 'deny', rules: groups.value.deny },
    { decision: 'allow', rules: groups.value.allow },
    { decision: 'ask', rules: groups.value.ask },
  ],
);
const selectedRule = computed<AppPermissionRule | undefined>(() =>
  selectedKey.value === null
    ? undefined
    : rules.value.find((rule) => ruleKey(rule) === selectedKey.value),
);
const selectedFacet = computed(() =>
  selectedRule.value === undefined ? undefined : ruleFacetPreview(selectedRule.value),
);

/** The draft's live facet preview (null while it does not parse — the
 *  pre-check the engine re-validates authoritatively). */
const draftFacet = computed(() => parseRuleFacet(draftPattern.value));
const draftValid = computed(() =>
  validateDraft({ decision: draftDecision.value, pattern: draftPattern.value }),
);

/** Stable row key — {pattern, decision} is the engine's removal address. */
function ruleKey(rule: AppPermissionRule): string {
  return `${rule.decision}:${rule.pattern}`;
}

// -------------------------------------------------------------------------
// Load — re-run when the backend changes so the dev backend switcher
// refetches and the v3 gate re-evaluates.
// -------------------------------------------------------------------------
/** Monotonic load token: a response from a superseded load is discarded
 *  instead of overwriting newer rows. */
let loadSeq = 0;

async function load(opts: { silent?: boolean } = {}): Promise<void> {
  const seq = ++loadSeq;
  error.value = '';
  if (!isV3.value) {
    rules.value = [];
    selectedKey.value = null;
    loading.value = false;
    return;
  }
  // A silent refresh keeps the current rows on screen instead of flashing
  // the full-panel spinner.
  if (!opts.silent) loading.value = true;
  try {
    const rows = await api.listPermissionRules();
    if (seq !== loadSeq) return;
    rules.value = rows;
    if (selectedKey.value !== null && !rows.some((rule) => ruleKey(rule) === selectedKey.value)) {
      selectedKey.value = null;
    }
  } catch (e) {
    if (seq === loadSeq) error.value = e instanceof Error ? e.message : String(e);
  } finally {
    if (seq === loadSeq) loading.value = false;
  }
}

watch(
  () => backend.value,
  () => { void load(); },
  { immediate: true },
);

// -------------------------------------------------------------------------
// Row actions
// -------------------------------------------------------------------------
async function onAdd(): Promise<void> {
  if (!draftValid.value || adding.value) return;
  adding.value = true;
  // Supersede any in-flight load: a slower listRules response must not
  // clobber the fresh post-write list below.
  const addSeq = ++loadSeq;
  try {
    const reason = draftReason.value.trim();
    const updated = await api.addPermissionRule({
      decision: draftDecision.value,
      pattern: draftPattern.value.trim(),
      ...(reason !== '' ? { reason } : {}),
    });
    rules.value = updated;
    draftPattern.value = '';
    draftReason.value = '';
    error.value = '';
  } catch (e) {
    // Action failure — surface through the operation-failure notice like
    // the other panel actions (CronPanel precedent); the inline `error`
    // banner stays load-scoped.
    client.pushOperationFailure('addPermissionRule', e);
  } finally {
    adding.value = false;
    // Clear the spinner only when this write is still the newest operation
    // — a load started later keeps its own spinner.
    if (addSeq === loadSeq) loading.value = false;
  }
}

async function onRemove(rule: AppPermissionRule): Promise<void> {
  const ok = await confirm({
    title: t('permissionRules.confirmDeleteTitle'),
    message: t('permissionRules.confirmDeleteBody', { pattern: rule.pattern }),
    variant: 'danger',
  });
  if (!ok) return;
  removingKey.value = ruleKey(rule);
  // Supersede any in-flight load (same race as onAdd).
  const removeSeq = ++loadSeq;
  try {
    const updated = await api.removePermissionRule(rule.pattern, rule.decision);
    rules.value = updated;
    if (selectedKey.value === ruleKey(rule)) selectedKey.value = null;
  } catch (e) {
    client.pushOperationFailure('removePermissionRule', e);
  } finally {
    removingKey.value = null;
    // Same guarded reset as onAdd.
    if (removeSeq === loadSeq) loading.value = false;
  }
}
</script>

<template>
  <div class="rules-panel">
    <!-- Header -->
    <div class="panel-head">
      <div class="panel-kicker">{{ t('settings.tabs.rules') }}</div>
      <h4 class="panel-title">{{ t('permissionRules.title') }}</h4>
      <p class="panel-desc">{{ t('permissionRules.desc') }}</p>
    </div>

    <!-- Toolbar: rules can change outside this tab (an always-allow upgrade
         in a session, a config.toml edit, the watcher's live push), so the
         refresh affordance re-reads the shared snapshot on demand. -->
    <div class="toolbar">
      <Button variant="secondary" size="sm" :disabled="!isV3 || loading" @click="load({ silent: true })">
        {{ t('permissionRules.refresh') }}
      </Button>
    </div>

    <!-- Add form -->
    <form class="add-form" @submit.prevent="onAdd">
      <div class="add-row">
        <SegmentedControl
          :model-value="draftDecision"
          :options="[
            { value: 'allow', label: t('permissionRules.decisionAllow') },
            { value: 'deny', label: t('permissionRules.decisionDeny') },
            { value: 'ask', label: t('permissionRules.decisionAsk') },
          ]"
          :aria-label="t('permissionRules.decisionLabel')"
          @update:model-value="draftDecision = $event as AppPermissionRuleDecision"
        />
        <Input
          v-model="draftPattern"
          class="add-pattern"
          :placeholder="t('permissionRules.patternPlaceholder')"
          :aria-label="t('permissionRules.patternLabel')"
        />
      </div>
      <Input
        v-model="draftReason"
        class="add-reason"
        :placeholder="t('permissionRules.reasonPlaceholder')"
        :aria-label="t('permissionRules.reasonLabel')"
      />
      <div class="add-foot">
        <span class="add-preview" :class="{ invalid: draftFacet === null && draftPattern.trim() !== '' }">
          <template v-if="draftFacet !== null">
            {{ t('permissionRules.previewTool', { tool: draftFacet.tool }) }}<template v-if="draftFacet.argPattern !== undefined"> · {{ t('permissionRules.previewArgs', { args: draftFacet.argPattern }) }}</template><template v-if="draftFacet.negated"> · {{ t('permissionRules.previewNegated') }}</template>
          </template>
          <template v-else-if="draftPattern.trim() !== ''">{{ t('permissionRules.previewInvalid') }}</template>
          <template v-else>{{ t('permissionRules.previewHint') }}</template>
        </span>
        <Button type="submit" size="sm" variant="primary" :disabled="!isV3 || !draftValid || adding">
          {{ adding ? t('permissionRules.adding') : t('permissionRules.add') }}
        </Button>
      </div>
    </form>

    <div v-if="error" class="error-msg">{{ error }}</div>

    <!-- v3-only honest degrade: no fake rows on the v1/v2 REST face -->
    <Banner v-if="!isV3" variant="info">{{ t('permissionRules.v3Only') }}</Banner>

    <EmptyState v-else-if="loading">
      <Spinner />
    </EmptyState>

    <EmptyState
      v-else-if="rules.length === 0"
      :title="t('permissionRules.empty')"
      :hint="t('permissionRules.emptyHint')"
    >
      <template #icon><Icon name="lock" /></template>
    </EmptyState>

    <div v-else class="master-detail">
      <!-- Left: grouped rule list -->
      <div class="list-pane" :class="{ 'hidden-mobile': selectedRule !== undefined }">
        <div class="list-scroll">
          <template v-for="group in groupedRows" :key="group.decision">
            <button
              v-for="rule in group.rules"
              :key="ruleKey(rule)"
              type="button"
              class="list-row"
              :class="{ selected: selectedKey === ruleKey(rule) }"
              :aria-current="selectedKey === ruleKey(rule) ? 'true' : undefined"
              @click="selectedKey = ruleKey(rule)"
            >
              <div class="list-row-main">
                <span class="list-row-pattern" :title="rule.pattern">{{ rule.pattern }}</span>
                <span v-if="rule.reason" class="list-row-reason" :title="rule.reason">{{ rule.reason }}</span>
              </div>
              <Badge :variant="decisionBadgeVariant(rule.decision)" size="sm">
                {{ t(`permissionRules.decision_${rule.decision}`) }}
              </Badge>
            </button>
          </template>
        </div>
      </div>

      <!-- Right: detail pane (selected row) -->
      <div class="detail-pane" :class="{ 'hidden-mobile': selectedRule === undefined }">
        <div v-if="selectedRule === undefined" class="empty-state">
          <Icon name="lock" size="lg" />
          <p>{{ t('permissionRules.selectPrompt') }}</p>
        </div>

        <template v-else>
          <div class="detail-header">
            <button type="button" class="back-btn" @click="selectedKey = null">
              <Icon name="arrow-left" size="sm" />
              <span>{{ t('permissionRules.backToList') }}</span>
            </button>
            <span class="detail-pattern">{{ selectedRule.pattern }}</span>
          </div>
          <div class="detail-scroll">
            <!-- Facet decomposition (preview mirror of the engine DSL) -->
            <div class="detail-grid">
              <div class="detail-field">
                <span class="detail-label">{{ t('permissionRules.facetTool') }}</span>
                <span class="detail-value mono">{{ selectedFacet?.tool }}</span>
              </div>
              <div class="detail-field">
                <span class="detail-label">{{ t('permissionRules.facetArgs') }}</span>
                <span class="detail-value mono">{{ selectedFacet?.argPattern ?? t('permissionRules.facetAnyArgs') }}</span>
              </div>
              <div class="detail-field">
                <span class="detail-label">{{ t('permissionRules.facetNegated') }}</span>
                <span class="detail-value">{{ selectedFacet?.negated ? t('permissionRules.yes') : t('permissionRules.no') }}</span>
              </div>
              <div class="detail-field">
                <span class="detail-label">{{ t('permissionRules.decisionLabel') }}</span>
                <Badge :variant="decisionBadgeVariant(selectedRule.decision)" size="sm">
                  {{ t(`permissionRules.decision_${selectedRule.decision}`) }}
                </Badge>
              </div>
              <div v-if="selectedRule.reason" class="detail-field">
                <span class="detail-label">{{ t('permissionRules.reasonLabel') }}</span>
                <span class="detail-value">{{ selectedRule.reason }}</span>
              </div>
            </div>
            <p class="detail-hint">{{ t('permissionRules.facetHint') }}</p>
            <div class="detail-actions">
              <Button
                variant="danger-soft"
                size="sm"
                :disabled="removingKey === ruleKey(selectedRule)"
                @click="onRemove(selectedRule)"
              >
                <Spinner v-if="removingKey === ruleKey(selectedRule)" size="sm" />
                {{ t('permissionRules.delete') }}
              </Button>
            </div>
          </div>
        </template>
      </div>
    </div>
  </div>
</template>

<style scoped>
.rules-panel { display: flex; flex-direction: column; height: 100%; gap: var(--space-3); }

.panel-head { margin-bottom: var(--space-2); }
.panel-kicker { font-size: var(--text-xs); letter-spacing: 0.08em; text-transform: uppercase; color: var(--color-text-faint); margin-bottom: var(--space-1); }
.panel-title { margin: 0 0 var(--space-2); font-family: var(--font-ui); font-size: var(--text-2xl); font-weight: var(--weight-semibold); letter-spacing: -0.01em; color: var(--color-text); }
.panel-desc { margin: 0; font-family: var(--font-ui); font-size: var(--text-sm); line-height: var(--leading-normal); color: var(--color-text-muted); max-width: 560px; }

/* --- Add form --- */
.add-form {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  padding: var(--space-3);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
}
.add-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  flex-wrap: wrap;
}
.add-pattern { flex: 1 1 240px; min-width: 0; }
.add-reason { max-width: 480px; }
.add-foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
}
.add-preview {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  min-width: 0;
  overflow-wrap: anywhere;
}
.add-preview.invalid { color: var(--color-danger); }

.toolbar {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  flex-wrap: wrap;
}

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
  align-items: center;
  gap: var(--space-2);
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

.list-row-pattern {
  font-family: var(--font-mono);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text);
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.list-row-reason {
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
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

.detail-pattern {
  font-family: var(--font-mono);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
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
  overflow-wrap: anywhere;
}
.detail-value.mono { font-family: var(--font-mono); }

.detail-hint {
  margin: 0;
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  line-height: var(--leading-normal);
  color: var(--color-text-muted);
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
  .add-foot {
    flex-direction: column;
    align-items: stretch;
  }
  .add-foot .ui-button {
    width: 100%;
    min-height: 46px;
  }
  /* The decision SegmentedControl is a primary input of the add form —
     keep the ≥44px tap-target rule on mobile. */
  .add-row :deep(.ui-seg__item) {
    min-height: 44px;
  }
}
</style>
