<!-- apps/mirri-web/src/components/settings/models/ProviderDetail.vue -->
<!-- Right pane: a selected provider's metadata + its model list + the model
     edit form for the selected model. Handles refresh / delete / default-model
     / add-model / delete-model inline. -->
<script setup lang="ts">
import { computed, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppModelAlias, AppProvider } from '../../../api/types';
import { getMirriWebApi } from '../../../api';
import { formatTokens } from '../../../lib/formatTokens';
import Button from '../../ui/Button.vue';
import Badge from '../../ui/Badge.vue';
import IconButton from '../../ui/IconButton.vue';
import Icon from '../../ui/Icon.vue';
import Input from '../../ui/Input.vue';
import Tooltip from '../../ui/Tooltip.vue';
import ModelEditForm from './ModelEditForm.vue';
import { useConfirmDialog } from '../../../composables/useConfirmDialog';

const { t } = useI18n();
const { confirm } = useConfirmDialog();
const api = getMirriWebApi();

const props = defineProps<{
  provider: AppProvider;
  /** All model aliases keyed by "provider/model". */
  models: Record<string, AppModelAlias>;
  /** Global default model id. */
  defaultModel?: string;
  refreshing?: boolean;
}>();

const emit = defineEmits<{
  refresh: [providerId: string];
  delete: [providerId: string];
  /** Models/provider changed — parent should reload. */
  changed: [];
  /** Set as global default model. */
  setDefaultModel: [modelId: string];
  /** Model alias saved — parent should update its models ref. */
  saved: [modelId: string, patch: Partial<AppModelAlias>];
}>();

// -------------------------------------------------------------------------
// Provider's models
// -------------------------------------------------------------------------
const providerModels = computed<Array<{ id: string; alias: AppModelAlias }>>(() => {
  return Object.entries(props.models)
    .filter(([, alias]) => alias.provider === props.provider.id)
    .map(([id, alias]) => ({ id, alias }))
    .toSorted((a, b) => (a.alias.displayName ?? a.id).localeCompare(b.alias.displayName ?? b.id));
});

const selectedModelId = ref<string | null>(null);
const selectedModel = computed<AppModelAlias | undefined>(() =>
  selectedModelId.value !== null ? props.models[selectedModelId.value] : undefined,
);
const search = ref('');

const filteredModels = computed(() => {
  const q = search.value.toLowerCase().trim();
  if (!q) return providerModels.value;
  return providerModels.value.filter((m) => {
    const name = (m.alias.displayName ?? m.alias.model).toLowerCase();
    return name.includes(q) || m.id.toLowerCase().includes(q);
  });
});

function selectModel(id: string): void {
  selectedModelId.value = id;
}

// -------------------------------------------------------------------------
// Actions
// -------------------------------------------------------------------------
async function onDeleteProvider(): Promise<void> {
  if (await confirm({ title: t('settings.models.deleteProvider'), message: t('settings.models.confirmDeleteProvider', { name: props.provider.type }), variant: 'danger' })) {
    emit('delete', props.provider.id);
  }
}

async function onDeleteModel(modelId: string): Promise<void> {
  if (await confirm({ title: t('settings.models.deleteModel'), message: t('settings.models.confirmDeleteModel', { name: modelId }), variant: 'danger' })) {
    try {
      await api.deleteModel(modelId);
      if (selectedModelId.value === modelId) selectedModelId.value = null;
      emit('changed');
    } catch (error) {
      console.error('deleteModel failed', error);
    }
  }
}

// -------------------------------------------------------------------------
// Add-model inline form (manual model for this provider)
// -------------------------------------------------------------------------
const showAddModel = ref(false);
const newModelId = ref('');
const newModelSaving = ref(false);
const newModelError = ref('');

function openAddModel(): void {
  newModelId.value = '';
  newModelError.value = '';
  showAddModel.value = true;
}

async function submitAddModel(): Promise<void> {
  const raw = newModelId.value.trim();
  if (!raw) {
    newModelError.value = t('settings.models.errModelId');
    return;
  }
  const fullId = `${props.provider.id}/${raw}`;
  if (props.models[fullId] !== undefined) {
    newModelError.value = t('settings.models.errModelExists');
    return;
  }
  newModelSaving.value = true;
  try {
    await api.setConfig({
      models: {
        [fullId]: {
          provider: props.provider.id,
          model: raw,
          maxContextSize: 128000,
        },
      },
    });
    showAddModel.value = false;
    selectedModelId.value = fullId;
    emit('changed');
  } catch (error) {
    newModelError.value = error instanceof Error ? error.message : String(error);
  } finally {
    newModelSaving.value = false;
  }
}

function statusColor(status: AppProvider['status']): string {
  if (status === 'connected') return 'var(--color-success)';
  if (status === 'error') return 'var(--color-danger)';
  return 'var(--color-text-faint)';
}

/** Capabilities to show as badges — common ones (thinking, tool_use) are
 *  omitted from the list view to reduce noise; they're still editable in the
 *  detail form. */
const HIDDEN_CAPS = new Set(['thinking', 'tool_use']);
function caps(alias: AppModelAlias): string[] {
  return (alias.capabilities ?? []).filter((c) => !HIDDEN_CAPS.has(c));
}
</script>

<template>
  <div class="pvd">
    <!-- Provider header -->
    <header class="pvd-head">
      <div class="pvd-head-main">
        <span class="status-dot" :style="{ background: statusColor(provider.status) }" />
        <div class="pvd-head-info">
          <span class="pvd-type">{{ provider.type }}</span>
          <span v-if="provider.baseUrl" class="pvd-url">{{ provider.baseUrl }}</span>
          <div class="pvd-head-meta">
            <Badge :variant="provider.hasApiKey ? 'success' : 'neutral'" size="sm">
              {{ provider.hasApiKey ? t('settings.models.keySet') : t('settings.models.keyNotSet') }}
            </Badge>
            <span class="pvd-count">{{ providerModels.length }} {{ t('settings.models.modelsUnit') }}</span>
          </div>
        </div>
      </div>
      <div class="pvd-head-actions">
        <Tooltip :text="t('settings.models.refreshProvider')">
          <Button variant="secondary" size="sm" :loading="refreshing" @click="emit('refresh', provider.id)">
            <Icon name="undo" size="sm" />
          </Button>
        </Tooltip>
        <Tooltip :text="t('settings.models.deleteProvider')">
          <Button variant="danger-soft" size="sm" @click="onDeleteProvider">
            <Icon name="close" size="sm" />
          </Button>
        </Tooltip>
      </div>
    </header>

    <!-- Inline add-model -->
    <div v-if="showAddModel" class="pvd-add-model">
      <Field :label="t('settings.models.fieldModelId')" :hint="t('settings.models.fieldModelIdHint', { provider: provider.id })">
        <Input v-model="newModelId" :placeholder="'model-name'" @keydown.enter.prevent="submitAddModel" />
      </Field>
      <div v-if="newModelError" class="pvd-add-err">{{ newModelError }}</div>
      <div class="pvd-add-btns">
        <Button variant="primary" size="sm" :loading="newModelSaving" @click="submitAddModel">{{ t('settings.models.add') }}</Button>
        <Button variant="secondary" size="sm" @click="showAddModel = false">{{ t('common.cancel') }}</Button>
      </div>
    </div>

    <!-- Search + add model button on one row -->
    <div class="pvd-search-row">
      <Input v-model="search" size="sm" :placeholder="t('settings.models.searchModels')" autocomplete="off" />
      <Tooltip :text="t('settings.models.addModel')">
        <Button variant="ghost" size="sm" class="pvd-add-btn" @click="openAddModel">
          <Icon name="plus" size="sm" />
        </Button>
      </Tooltip>
    </div>

    <div class="pvd-model-list">
      <div v-if="filteredModels.length === 0" class="pvd-empty">{{ t('settings.models.noModels') }}</div>
      <div
        v-for="m in filteredModels"
        :key="m.id"
        class="pvd-model-row"
        :class="{ on: m.id === selectedModelId, 'is-default': m.id === defaultModel }"
        @click="selectModel(m.id)"
      >
        <span class="pvd-model-check">
          <Icon v-if="m.id === selectedModelId" name="chevron-down" size="sm" />
          <Icon v-else name="chevron-right" size="sm" />
        </span>
        <span class="pvd-model-main">
          <span class="pvd-model-name">{{ m.alias.displayName ?? m.alias.model }}</span>
          <Badge variant="solid" size="sm" class="pvd-model-alias">{{ m.id }}</Badge>
          <Tooltip v-if="m.alias.description" :text="t('settings.models.llmVisibleHint')">
            <Badge variant="success" size="sm" class="pvd-model-llm">{{ t('settings.models.llmVisible') }}</Badge>
          </Tooltip>
        </span>
        <span class="pvd-model-caps">
          <Badge v-for="cap in caps(m.alias).slice(0, 3)" :key="cap" variant="info" size="sm">{{ cap }}</Badge>
        </span>
        <span class="pvd-model-ctx">{{ m.alias.maxContextSize ? formatTokens(m.alias.maxContextSize) : '—' }}</span>
        <Tooltip :text="t('settings.models.deleteModel')">
          <IconButton size="sm" :label="t('settings.models.deleteModel')" @click.stop="onDeleteModel(m.id)">
            <Icon name="close" size="sm" />
          </IconButton>
        </Tooltip>
      </div>
    </div>

    <!-- Selected model edit form -->
    <div v-if="selectedModel" class="pvd-edit">
      <ModelEditForm
        :model-id="selectedModelId!"
        :alias="selectedModel"
        @save-error="(id, err) => console.error('model save failed', id, err)"
        @saved="(id, patch) => emit('saved', id, patch)"
      />
      <div class="pvd-default">
        <Button
          variant="secondary"
          size="sm"
          :disabled="selectedModelId === defaultModel"
          @click="selectedModelId && emit('setDefaultModel', selectedModelId)"
        >
          {{ selectedModelId === defaultModel ? t('settings.models.isDefaultModel') : t('settings.models.setAsDefault') }}
        </Button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.pvd { display: flex; flex-direction: column; gap: var(--space-3); min-width: 0; }

.pvd-head { display: flex; align-items: flex-start; justify-content: space-between; gap: var(--space-3); padding-bottom: var(--space-3); border-bottom: 1px solid var(--color-line); }
.pvd-head-main { display: flex; align-items: flex-start; gap: var(--space-2); min-width: 0; }
.status-dot { width: 8px; height: 8px; border-radius: 50%; flex: none; margin-top: 6px; }
.pvd-head-info { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.pvd-type { font-family: var(--font-ui); font-size: var(--text-base); font-weight: var(--weight-medium); color: var(--color-text); }
.pvd-url { font-family: var(--font-mono); font-size: var(--text-xs); color: var(--color-text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.pvd-head-meta { display: flex; align-items: center; gap: var(--space-2); margin-top: 2px; }
.pvd-count { font-size: var(--text-xs); color: var(--color-text-faint); }
.pvd-head-actions { display: flex; gap: var(--space-1); flex: none; }

.pvd-search-row { display: flex; align-items: center; gap: var(--space-2); }
.pvd-search-row > :first-child { flex: 1; min-width: 0; }
.pvd-add-btn { flex: none; }

.pvd-add-model { display: flex; flex-direction: column; gap: var(--space-2); padding: var(--space-3); border: 1px solid var(--color-accent-bd); border-radius: var(--radius-md); background: var(--color-accent-soft); }
.pvd-add-err { font-size: var(--text-sm); color: var(--color-danger); }
.pvd-add-btns { display: flex; gap: var(--space-2); }

.pvd-model-list { display: flex; flex-direction: column; gap: 1px; max-height: 320px; overflow-y: auto; }
.pvd-model-row { display: flex; align-items: center; gap: var(--space-2); padding: var(--space-2); border-radius: var(--radius-md); cursor: pointer; transition: background var(--duration-fast) var(--ease-out); }
.pvd-model-row:hover { background: var(--color-surface-sunken); }
.pvd-model-row.on { background: var(--color-accent-soft); box-shadow: inset 0 0 0 1px var(--color-accent-bd); }
.pvd-model-row.is-default .pvd-model-name::after { content: ' ★'; color: var(--color-accent); }
.pvd-model-check { flex: none; color: var(--color-text-faint); display: flex; align-items: center; }
.pvd-model-main { flex: 1; min-width: 0; display: flex; align-items: center; gap: var(--space-1); }
.pvd-model-name { font-size: var(--text-sm); font-weight: var(--weight-medium); color: var(--color-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
.pvd-model-alias { font-family: var(--font-mono); font-size: 10px; flex: none; max-width: 180px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.pvd-model-llm { flex: none; }
.pvd-model-caps { display: flex; gap: 3px; flex: none; }
.pvd-model-ctx { flex: none; font-family: var(--font-mono); font-size: var(--text-xs); color: var(--color-text-muted); }
.pvd-empty { padding: var(--space-4); text-align: center; color: var(--color-text-faint); font-size: var(--text-sm); }

.pvd-edit { border-top: 1px solid var(--color-line); padding-top: var(--space-2); }
.pvd-default { padding-top: var(--space-2); display: flex; justify-content: flex-end; }

@media (max-width: 640px) {
  .pvd-model-caps { display: none; }
}
</style>
