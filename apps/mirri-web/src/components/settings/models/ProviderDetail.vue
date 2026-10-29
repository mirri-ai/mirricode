<!-- apps/mirri-web/src/components/settings/models/ProviderDetail.vue -->
<!-- Right pane: a selected provider's metadata + its model list. Model
     add/edit open in modal dialogs above the fullscreen settings overlay. -->
<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppModelAlias, AppProvider, ProviderCredentialInfo } from '../../../api/types';
import { getMirriWebApi } from '../../../api';
import { modelCountForProvider } from '../../../lib/modelProviderCreateGuide';
import { formatTokens } from '../../../lib/formatTokens';
import { resolveAlias } from '../../../lib/resolveAlias';
import { initModelSelection, toggleAllVisibleModels, toggleModelSelection } from '../../../lib/modelSelection';
import {
  buildCredentialValue,
  buildEnvRefConfigPatch,
  credentialBadgeFor,
  isEnvRef,
  type CredentialFormKind,
} from '../../../lib/providerCredentials';
import Button from '../../ui/Button.vue';
import Badge from '../../ui/Badge.vue';
import Checkbox from '../../ui/Checkbox.vue';
import IconButton from '../../ui/IconButton.vue';
import Icon from '../../ui/Icon.vue';
import Input from '../../ui/Input.vue';
import Tooltip from '../../ui/Tooltip.vue';
import Dialog from '../../ui/Dialog.vue';
import Field from '../../ui/Field.vue';
import ModelEditDialog from './ModelEditDialog.vue';
import { useConfirmDialog } from '../../../composables/useConfirmDialog';
import { isCompositionKeyEvent } from '../../../lib/keyboard';

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
  /** One-shot counter: each bump opens this provider's add-model dialog.
   *  Bumped by the parent after selecting a freshly created zero-model
   *  provider (first-run guidance). */
  addModelRequest?: number;
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
  /** A deletion failed — parent should surface the error (banner). */
  deleteFailed: [message: string];
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
// Multi-select batch deletion (B8) — the selection state machine itself
// lives in lib/modelSelection (pure functions); this wires it up.
// -------------------------------------------------------------------------
const selection = ref(initModelSelection());
const visibleModelIds = computed(() => filteredModels.value.map((m) => m.id));
const allVisibleSelected = computed(
  () => visibleModelIds.value.length > 0 && visibleModelIds.value.every((id) => selection.value.ids.has(id)),
);
const someVisibleSelected = computed(() =>
  visibleModelIds.value.some((id) => selection.value.ids.has(id)),
);

// The component is not keyed by provider in the parent — switching
// providers reuses the instance, so stale checks must be dropped here
// or the bulk bar would count the previous provider's rows.
watch(
  () => props.provider.id,
  () => {
    selection.value = initModelSelection();
  },
);

function onToggleAllVisible(): void {
  selection.value = toggleAllVisibleModels(selection.value, visibleModelIds.value);
}

function onToggleModel(id: string): void {
  selection.value = toggleModelSelection(selection.value, id);
}

function clearSelection(): void {
  selection.value = initModelSelection();
}

async function onDeleteSelected(): Promise<void> {
  const ids = [...selection.value.ids].filter((id) =>
    providerModels.value.some((m) => m.id === id),
  );
  if (ids.length === 0) return;
  // The confirm dialog's `action` rethrows on failure — keep the API call
  // OUT of `action` and try/catch it here (the RetentionPanel console.error
  // pattern), so a failed delete surfaces without an unhandled rejection.
  const confirmed = await confirm({
    title: t('settings.models.deleteSelected'),
    message: t('settings.models.confirmDeleteSelected', { count: ids.length }),
    variant: 'danger',
  });
  if (!confirmed) return;
  try {
    await api.deleteModel(ids);
  } catch (error) {
    console.error('deleteModel failed', error);
    emit('deleteFailed', error instanceof Error ? error.message : String(error));
    return;
  }
  if (editingModelId.value !== null && ids.includes(editingModelId.value)) {
    editingModelId.value = null;
  }
  selection.value = initModelSelection();
  emit('changed');
}

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
  if (await confirm({ title: t('settings.models.deleteProvider'), message: t('settings.models.confirmDeleteProvider', { name: props.provider.id }), variant: 'danger' })) {
    emit('delete', props.provider.id);
  }
}

async function onDeleteModel(modelId: string): Promise<void> {
  if (await confirm({ title: t('settings.models.deleteModel'), message: t('settings.models.confirmDeleteModel', { name: modelId }), variant: 'danger' })) {
    try {
      await api.deleteModel([modelId]);
      if (editingModelId.value === modelId) editingModelId.value = null;
      emit('changed');
    } catch (error) {
      console.error('deleteModel failed', error);
      emit('deleteFailed', error instanceof Error ? error.message : String(error));
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

// First-run guidance: the parent selects a freshly created zero-model
// provider and bumps `addModelRequest` — open the add-model dialog for it.
// Non-immediate so an initial mount of the detail pane never auto-opens.
watch(
  () => props.addModelRequest,
  (request) => {
    if (request === undefined || request <= 0) return;
    // Guard on the actual model count: only an empty provider is guided.
    if (modelCountForProvider(props.provider.id, props.models) > 0) return;
    openAddModel();
  },
);

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
// Credentials — the credential-store face (Task 4/5 of the closeout)
// -------------------------------------------------------------------------
/** This provider's stored credential row (undefined = nothing stored). */
const credential = ref<ProviderCredentialInfo | undefined>(undefined);
const credentialLoading = ref(false);

/** The header badge view over the store row + the legacy env-ref tier. */
const credentialBadge = computed(() => credentialBadgeFor(credential.value, props.provider));

/** The D.1 guidance text under the badge when a reconfigure is needed. */
const credentialGuidance = computed(() =>
  credential.value?.status === 'needs_reconfigure'
    ? t('settings.models.keyNeedsReconfigureHint')
    : null,
);

/** In-flight load sequence: switching providers reuses this component
 *  instance, so a slow earlier `credentialList` answer must never
 *  overwrite the row of the provider now on screen — every load takes a
 *  ticket and ignores answers that are no longer the newest. */
let loadSeq = 0;

async function loadCredential(): Promise<void> {
  const seq = ++loadSeq;
  credentialLoading.value = true;
  try {
    const rows = await api.credentialList(props.provider.id);
    // The provider may have been switched while the call was in flight —
    // drop a stale row (the next watch/load writes the right one).
    if (seq !== loadSeq) return;
    const row = rows.find((r) => r.providerId === props.provider.id);
    credential.value = row ?? undefined;
  } catch (error) {
    if (seq !== loadSeq) return;
    // The credential face is a degrade-not-die view (the legacy backend
    // rejects it): log and show the no-entry state.
    console.error('credentialList failed', error);
    credential.value = undefined;
  } finally {
    if (seq === loadSeq) credentialLoading.value = false;
  }
}

onMounted(() => {
  // G.7: the store can change under us (another surface, the CLI) — the
  // credential.updated notification drives the refresh. Scoped to this
  // provider: another provider's write does not touch this page's row.
  offCredentialUpdated = api.onCredentialUpdated((providerId) => {
    if (providerId === undefined || providerId === props.provider.id) {
      void loadCredential();
    }
  });
  void loadCredential();
});

let offCredentialUpdated: (() => void) | null = null;
onUnmounted(() => {
  offCredentialUpdated?.();
  offCredentialUpdated = null;
});

// Switching providers reuses the component instance (no key in the
// parent) — reload the credential row for the newly selected provider.
watch(
  () => props.provider.id,
  () => {
    credential.value = undefined;
    void loadCredential();
  },
);

// -------------------------------------------------------------------------
// Edit credential dialog (the store's write face: API key / Azure CLI)
// -------------------------------------------------------------------------
const showEditKey = ref(false);
/** The credential form kind — 'key' or the external 'azure-cli' source. */
const editKeyFormKind = ref<CredentialFormKind>('key');
const editKeyValue = ref('');
const editKeyResourceName = ref('');
const editKeySaving = ref(false);
const editKeyError = ref('');

/** Whether the current key input is a pure env-var ref — the config-side
 *  tier, which keeps its setConfig path (env references are configuration
 *  facts, not credentials). */
const editKeyIsEnvRef = computed(() => isEnvRef(editKeyValue.value));

function openEditKey(): void {
  // Never pre-filled: the store is write-only from the web (F.2/G.6 — no
  // plaintext ever reads back over RPC), so the dialog always starts blank.
  editKeyFormKind.value = 'key';
  editKeyValue.value = '';
  editKeyResourceName.value = '';
  editKeyError.value = '';
  showEditKey.value = true;
}

async function submitEditKey(): Promise<void> {
  editKeyError.value = '';
  // The env-ref tier only exists on the key form — an azure-cli form has
  // no env-ref reading of the resource name.
  if (editKeyFormKind.value === 'key' && editKeyIsEnvRef.value) {
    // Write the provider's env sub-table row for the type's conventional
    // key — the apiKey field is never touched.
    const patch = buildEnvRefConfigPatch(props.provider, editKeyValue.value);
    if (patch === null) {
      editKeyError.value = t('settings.models.errApiKeyEnvUnsupported');
      return;
    }
    editKeySaving.value = true;
    try {
      await api.setConfig(patch);
      showEditKey.value = false;
      emit('changed');
    } catch (error) {
      editKeyError.value = error instanceof Error ? error.message : String(error);
    } finally {
      editKeySaving.value = false;
    }
    return;
  }
  const value = buildCredentialValue({
    formKind: editKeyFormKind.value,
    key: editKeyValue.value,
    resourceName: editKeyResourceName.value,
  });
  if (value === null) {
    editKeyError.value = t('settings.models.errApiKey');
    return;
  }
  editKeySaving.value = true;
  try {
    await api.credentialSet(props.provider.id, value);
    showEditKey.value = false;
    // The credential.updated notification also refreshes, but a direct
    // read-back avoids depending on the notification's delivery.
    await loadCredential();
  } catch (error) {
    editKeyError.value = error instanceof Error ? error.message : String(error);
  } finally {
    editKeySaving.value = false;
  }
}

async function onRemoveCredential(): Promise<void> {
  if (
    await confirm({
      title: t('settings.models.removeCredential'),
      message: t('settings.models.confirmRemoveCredential', { name: props.provider.id }),
      variant: 'danger',
    })
  ) {
    try {
      await api.credentialRemove(props.provider.id);
    } catch (error) {
      console.error('credentialRemove failed', error);
      emit('deleteFailed', error instanceof Error ? error.message : String(error));
      return;
    }
    await loadCredential();
  }
}

function onKeyEditKeydown(e: KeyboardEvent): void {
  if (isCompositionKeyEvent(e)) return;
  if (e.key === 'Enter') {
    e.preventDefault();
    submitEditKey();
  }
}
function onAddModelKeydown(e: KeyboardEvent): void {
  if (isCompositionKeyEvent(e)) return;
  if (e.key === 'Enter') {
    e.preventDefault();
    submitAddModel();
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
            <Badge :variant="credentialBadge.variant" size="sm">
              <template v-if="credentialBadge.envRef">{{ credentialBadge.envRef }}</template>
              <template v-else>{{ t(`settings.models.${credentialBadge.labelKey}`) }}</template>
            </Badge>
            <Tooltip v-if="credential?.type === 'external'" :text="credential.externalAvailable ? t('settings.models.keyExternalAvailable') : t('settings.models.keyExternalUnavailable')">
              <Badge :variant="credential.externalAvailable ? 'success' : 'neutral'" size="sm">
                {{ credential.externalAvailable ? t('settings.models.keyExternalAvailable') : t('settings.models.keyExternalUnavailable') }}
              </Badge>
            </Tooltip>
            <span class="pvd-count">{{ providerModels.length }} {{ t('settings.models.modelsUnit') }}</span>
          </div>
          <div v-if="credentialGuidance" class="pvd-credential-guidance">{{ credentialGuidance }}</div>
        </div>
      </div>
      <div class="pvd-head-actions">
        <Tooltip :text="t('settings.models.editApiKey')">
          <Button variant="ghost" size="sm" @click="openEditKey">
            <Icon name="pencil" size="sm" />
          </Button>
        </Tooltip>
        <Tooltip v-if="credential" :text="t('settings.models.removeCredential')">
          <Button variant="danger-soft" size="sm" @click="onRemoveCredential">
            <Icon name="close" size="sm" />
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
      <Checkbox
        :model-value="allVisibleSelected"
        :indeterminate="someVisibleSelected && !allVisibleSelected"
        :aria-label="t('settings.models.selectAllModels')"
        @update:model-value="onToggleAllVisible()"
      />
      <span v-if="selection.ids.size > 0" class="pvd-bulk">
        {{ t('settings.models.selectedCount', { count: selection.ids.size }) }}
        <Button variant="danger-soft" size="sm" @click="onDeleteSelected">
          {{ t('settings.models.deleteSelected', { count: selection.ids.size }) }}
        </Button>
        <Button variant="ghost" size="sm" @click="clearSelection">
          {{ t('settings.models.clearSelection') }}
        </Button>
      </span>
      <Button variant="primary" size="md" class="pvd-add-btn" @click="openAddModel">
        <Icon name="plus" size="md" />
        <span class="pvd-add-label">{{ t('settings.models.addModel') }}</span>
      </Button>
    </div>

    <div class="pvd-model-list">
      <!-- Provider has no models at all — a large CTA instead of the plain
           empty text, so the next action is obvious. -->
      <div v-if="providerModels.length === 0" class="pvd-empty-cta">
        <span class="pvd-empty-text">{{ t('settings.models.noModels') }}</span>
        <Button variant="primary" size="lg" @click="openAddModel">
          <Icon name="plus" size="md" />
          {{ t('settings.models.addModel') }}
        </Button>
      </div>
      <!-- Models exist but the search filter matched none. -->
      <div v-else-if="filteredModels.length === 0" class="pvd-empty">{{ t('settings.models.noModels') }}</div>
      <div
        v-for="m in filteredModels"
        :key="m.id"
        class="pvd-model-row"
        :class="{ 'is-default': m.id === defaultModel }"
        @click="openEditModel(m.id)"
      >
        <span class="pvd-model-check" @click.stop>
          <Checkbox
            :model-value="selection.ids.has(m.id)"
            :aria-label="t('settings.models.selectModel', { name: m.alias.displayName ?? m.id })"
            @update:model-value="onToggleModel(m.id)"
          />
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

    <!-- Edit credential dialog -->
    <Dialog
      v-if="showEditKey"
      :open="true"
      :title="t('settings.models.editApiKey')"
      size="md"
      overlay-class="model-edit-overlay"
      @close="showEditKey = false"
    >
      <div class="pvd-add-form">
        <Field :label="t('settings.models.credentialKind')">
          <div class="pvd-kind-choice">
            <label class="pvd-kind-option">
              <input
                type="radio"
                name="credential-kind"
                value="key"
                :checked="editKeyFormKind === 'key'"
                @change="editKeyFormKind = 'key'"
              />
              {{ t('settings.models.credentialKindKey') }}
            </label>
            <label class="pvd-kind-option">
              <input
                type="radio"
                name="credential-kind"
                value="azure-cli"
                :checked="editKeyFormKind === 'azure-cli'"
                @change="editKeyFormKind = 'azure-cli'"
              />
              {{ t('settings.models.credentialKindAzureCli') }}
            </label>
          </div>
        </Field>
        <template v-if="editKeyFormKind === 'key'">
          <Field :label="t('settings.models.fieldApiKey')" :hint="editKeyIsEnvRef ? t('settings.models.apiKeyEnvRefHint') : t('settings.models.fieldApiKeyHint')">
            <Input
              v-model="editKeyValue"
              :type="editKeyIsEnvRef ? 'text' : 'password'"
              placeholder="sk-… or ${MY_VAR}"
              autocomplete="off"
              spellcheck="false"
              @keydown="onKeyEditKeydown"
            />
          </Field>
        </template>
        <template v-else>
          <Field :label="t('settings.models.fieldResourceName')" :hint="t('settings.models.fieldResourceNameHint')">
            <Input
              v-model="editKeyResourceName"
              :placeholder="'https://management.core.windows.net/'"
              autocomplete="off"
              spellcheck="false"
              @keydown="onKeyEditKeydown"
            />
          </Field>
        </template>
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
            @keydown="onAddModelKeydown"
          />
        </Field>
        <Field :label="t('settings.models.fieldModelName')" :hint="t('settings.models.fieldModelNameHint')">
          <Input
            v-model="newModelName"
            :placeholder="'GLM-5.2-Coding'"
            list="provider-models-list"
            autocomplete="off"
            spellcheck="false"
            @keydown="onAddModelKeydown"
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
.pvd-credential-guidance { margin-top: 2px; font-size: var(--text-xs); color: var(--color-danger); }
.pvd-head-actions { display: flex; gap: var(--space-1); flex: none; }

.pvd-kind-choice { display: flex; gap: var(--space-3); }
.pvd-kind-option { display: inline-flex; align-items: center; gap: var(--space-1); font-size: var(--text-sm); color: var(--color-text); cursor: pointer; }

.pvd-search-row { display: flex; align-items: center; gap: var(--space-2); }
.pvd-search-row > :first-child { flex: 1; min-width: 0; }
.pvd-add-btn { flex: none; }
.pvd-bulk { display: inline-flex; gap: var(--space-2); align-items: center; margin-left: var(--space-2); flex: none; }
.pvd-model-check { flex: none; display: inline-flex; align-items: center; }

/* Small screens: icon-only so the button doesn't crowd the search box. */
@media (max-width: 640px) {
  .pvd-add-label { display: none; }
}

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
.pvd-empty-cta { display: flex; flex-direction: column; align-items: center; gap: var(--space-4); padding: var(--space-6) var(--space-4); text-align: center; }
.pvd-empty-text { color: var(--color-text-faint); font-size: var(--text-sm); }

@media (max-width: 640px) {
  .pvd-model-caps { display: none; }
}
</style>
