<!-- apps/mirri-web/src/components/settings/models/ProviderDetail.vue -->
<!-- Right pane: a selected provider's metadata + its model list. Model
     add/edit open in modal dialogs above the fullscreen settings overlay. -->
<script setup lang="ts">
import { computed, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppModelAlias, AppProvider } from '../../../api/types';
import { getMirriWebApi } from '../../../api';
import { formatTokens } from '../../../lib/formatTokens';
import { resolveAlias } from '../../../lib/resolveAlias';
import Button from '../../ui/Button.vue';
import Badge from '../../ui/Badge.vue';
import IconButton from '../../ui/IconButton.vue';
import Icon from '../../ui/Icon.vue';
import Input from '../../ui/Input.vue';
import Tooltip from '../../ui/Tooltip.vue';
import Dialog from '../../ui/Dialog.vue';
import Field from '../../ui/Field.vue';
import ModelEditDialog from './ModelEditDialog.vue';
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
    .map(([id, alias]) => ({ id, alias: resolveAlias(alias) }))
    .toSorted((a, b) => (a.alias.displayName ?? a.id).localeCompare(b.alias.displayName ?? b.id));
});

const search = ref('');

const filteredModels = computed(() => {
  const q = search.value.toLowerCase().trim();
  if (!q) return providerModels.value;
  return providerModels.value.filter((m) => {
    const name = (m.alias.displayName ?? m.alias.model).toLowerCase();
    return name.includes(q) || m.id.toLowerCase().includes(q);
  });
});

// -------------------------------------------------------------------------
// Model edit dialog
// -------------------------------------------------------------------------
const editingModelId = ref<string | null>(null);
const editingModel = computed<AppModelAlias | undefined>(() =>
  editingModelId.value !== null ? props.models[editingModelId.value] : undefined,
);

function openEditModel(id: string): void {
  editingModelId.value = id;
}

function closeEditModel(): void {
  editingModelId.value = null;
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
      if (editingModelId.value === modelId) editingModelId.value = null;
      emit('changed');
    } catch (error) {
      console.error('deleteModel failed', error);
    }
  }
}

// -------------------------------------------------------------------------
// Add-model dialog (manual model for this provider)
// -------------------------------------------------------------------------
const showAddModel = ref(false);
const newModelAlias = ref('');
const newModelName = ref('');
const newModelSaving = ref(false);
const newModelError = ref('');

function openAddModel(): void {
  newModelAlias.value = '';
  newModelName.value = '';
  newModelError.value = '';
  showAddModel.value = true;
}

async function submitAddModel(): Promise<void> {
  const aliasRaw = newModelAlias.value.trim();
  const nameRaw = newModelName.value.trim();
  if (!aliasRaw) {
    newModelError.value = t('settings.models.errModelAlias');
    return;
  }
  if (aliasRaw.includes('/')) {
    newModelError.value = t('settings.models.errModelAliasSlash');
    return;
  }
  if (!nameRaw) {
    newModelError.value = t('settings.models.errModelName');
    return;
  }
  const fullId = `${props.provider.id}/${aliasRaw}`;
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
          model: nameRaw,
          maxContextSize: 128000,
        },
      },
    });
    showAddModel.value = false;
    emit('changed');
    // Open the edit dialog for the newly created model so the user can
    // configure it immediately.
    editingModelId.value = fullId;
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

// -------------------------------------------------------------------------
// Edit API key dialog
// -------------------------------------------------------------------------
const showEditKey = ref(false);
const editKeyValue = ref('');
const editKeySaving = ref(false);
const editKeyError = ref('');

/** Whether the current input in the edit-key dialog is a pure env-var ref. */
const editKeyIsEnvRef = computed(() =>
  /^\$\{(?:env:)?[A-Za-z_][A-Za-z0-9_]*\}$/.test(editKeyValue.value.trim()),
);
const editKeyInputType = computed(() => (editKeyIsEnvRef.value ? 'text' : 'password'));

/** Badge variant for the provider's API key status. */
const apiKeyBadgeVariant = computed<'success' | 'info' | 'neutral'>(() => {
  if (props.provider.apiKeyDisplay) return 'info';
  if (props.provider.hasApiKey) return 'success';
  return 'neutral';
});

function openEditKey(): void {
  // Pre-fill with the env ref if the provider uses one; literal keys are
  // never sent from the server, so leave the field empty for those.
  editKeyValue.value = props.provider.apiKeyDisplay ?? '';
  editKeyError.value = '';
  showEditKey.value = true;
}

async function submitEditKey(): Promise<void> {
  const value = editKeyValue.value.trim();
  if (!value) {
    editKeyError.value = t('settings.models.errApiKey');
    return;
  }
  editKeySaving.value = true;
  try {
    // Patch only the api_key field for this provider; the server deep-merges
    // the patch into the on-disk config.
    await api.setConfig({
      providers: {
        [props.provider.id]: { apiKey: value },
      },
    });
    showEditKey.value = false;
    emit('changed');
  } catch (error) {
    editKeyError.value = error instanceof Error ? error.message : String(error);
  } finally {
    editKeySaving.value = false;
  }
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
            <Badge :variant="apiKeyBadgeVariant" size="sm">
              <template v-if="provider.apiKeyDisplay">{{ provider.apiKeyDisplay }}</template>
              <template v-else-if="provider.hasApiKey">{{ t('settings.models.keySet') }}</template>
              <template v-else>{{ t('settings.models.keyNotSet') }}</template>
            </Badge>
            <span class="pvd-count">{{ providerModels.length }} {{ t('settings.models.modelsUnit') }}</span>
          </div>
        </div>
      </div>
      <div class="pvd-head-actions">
        <Tooltip :text="t('settings.models.editApiKey')">
          <Button variant="ghost" size="sm" @click="openEditKey">
            <Icon name="pencil" size="sm" />
          </Button>
        </Tooltip>
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
        :class="{ 'is-default': m.id === defaultModel }"
        @click="openEditModel(m.id)"
      >
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

    <!-- Edit API key dialog -->
    <Dialog
      v-if="showEditKey"
      :open="true"
      :title="t('settings.models.editApiKey')"
      size="md"
      overlay-class="model-edit-overlay"
      @close="showEditKey = false"
    >
      <div class="pvd-add-form">
        <Field :label="t('settings.models.fieldApiKey')" :hint="editKeyIsEnvRef ? t('settings.models.apiKeyEnvRefHint') : t('settings.models.fieldApiKeyHint')">
          <Input
            v-model="editKeyValue"
            :type="editKeyInputType"
            placeholder="sk-… or ${MY_VAR}"
            autocomplete="off"
            spellcheck="false"
            @keydown.enter.prevent="submitEditKey"
          />
        </Field>
        <div v-if="editKeyError" class="pvd-add-err">{{ editKeyError }}</div>
      </div>
      <template #foot>
        <Button variant="primary" size="sm" :loading="editKeySaving" @click="submitEditKey">{{ t('common.save') }}</Button>
        <Button variant="secondary" size="sm" @click="showEditKey = false">{{ t('common.cancel') }}</Button>
      </template>
    </Dialog>

    <!-- Add-model dialog -->
    <Dialog
      v-if="showAddModel"
      :open="true"
      :title="t('settings.models.addModel')"
      size="md"
      overlay-class="model-edit-overlay"
      @close="showAddModel = false"
    >
      <div class="pvd-add-form">
        <Field :label="t('settings.models.fieldModelAlias')" :hint="t('settings.models.fieldModelAliasHint', { provider: provider.id })">
          <Input
            v-model="newModelAlias"
            :placeholder="'expert'"
            autocomplete="off"
            spellcheck="false"
            @keydown.enter.prevent="submitAddModel"
          />
        </Field>
        <Field :label="t('settings.models.fieldModelName')" :hint="t('settings.models.fieldModelNameHint')">
          <Input
            v-model="newModelName"
            :placeholder="'GLM-5.2-Coding'"
            list="provider-models-list"
            autocomplete="off"
            spellcheck="false"
            @keydown.enter.prevent="submitAddModel"
          />
          <datalist id="provider-models-list">
            <option v-for="m in provider.models" :key="m" :value="m" />
          </datalist>
        </Field>
        <div v-if="newModelError" class="pvd-add-err">{{ newModelError }}</div>
      </div>
      <template #foot>
        <Button variant="primary" size="sm" :loading="newModelSaving" @click="submitAddModel">{{ t('settings.models.add') }}</Button>
        <Button variant="secondary" size="sm" @click="showAddModel = false">{{ t('common.cancel') }}</Button>
      </template>
    </Dialog>

    <!-- Model edit dialog -->
    <ModelEditDialog
      v-if="editingModel && editingModelId"
      :model-id="editingModelId"
      :alias="editingModel"
      :is-default="editingModelId === defaultModel"
      @close="closeEditModel"
      @saved="(id, patch) => emit('saved', id, patch)"
      @save-error="(id, err) => console.error('model save failed', id, err)"
      @set-default="(id) => emit('setDefaultModel', id)"
    />
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

.pvd-add-form { display: flex; flex-direction: column; gap: var(--space-2); }
.pvd-add-err { font-size: var(--text-sm); color: var(--color-danger); }

.pvd-model-list { display: flex; flex-direction: column; gap: 1px; max-height: 320px; overflow-y: auto; }
.pvd-model-row { display: flex; align-items: center; gap: var(--space-2); padding: var(--space-2); border-radius: var(--radius-md); cursor: pointer; transition: background var(--duration-fast) var(--ease-out); }
.pvd-model-row:hover { background: var(--color-surface-sunken); }
.pvd-model-row.is-default .pvd-model-name::after { content: ' ★'; color: var(--color-accent); }
.pvd-model-main { flex: 1; min-width: 0; display: flex; align-items: center; gap: var(--space-1); }
.pvd-model-name { font-size: var(--text-sm); font-weight: var(--weight-medium); color: var(--color-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
.pvd-model-alias { font-family: var(--font-mono); font-size: 10px; flex: none; max-width: 180px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.pvd-model-llm { flex: none; }
.pvd-model-caps { display: flex; gap: 3px; flex: none; }
.pvd-model-ctx { flex: none; font-family: var(--font-mono); font-size: var(--text-xs); color: var(--color-text-muted); }
.pvd-empty { padding: var(--space-4); text-align: center; color: var(--color-text-faint); font-size: var(--text-sm); }

@media (max-width: 640px) {
  .pvd-model-caps { display: none; }
}
</style>
