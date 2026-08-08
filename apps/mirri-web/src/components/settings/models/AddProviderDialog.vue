<!-- apps/mirri-web/src/components/settings/models/AddProviderDialog.vue -->
<!-- Add-provider dialog, v2-only (kap-server). Two paths:
     1. Catalog path — browse the models.dev catalog (server-proxied), pick one,
        fill API key / base URL; submitting POSTs /providers:import_catalog.
        Importing an id that already exists appends only new catalog models —
        existing aliases (incl. LLM-aware settings) are never rewritten and
        user-deleted models are never resurrected.
     2. Custom path — manual id/type/baseUrl/apiKey plus a ≥1-model editor for
        non-catalog providers; submitting POSTs /providers (v2 create). -->
<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppCatalogProvider, AppAddProviderModelInput } from '../../../api/types';
import { getMirriWebApi } from '../../../api';
import Dialog from '../../ui/Dialog.vue';
import Button from '../../ui/Button.vue';
import Input from '../../ui/Input.vue';
import Select from '../../ui/Select.vue';
import Field from '../../ui/Field.vue';
import Badge from '../../ui/Badge.vue';
import Spinner from '../../ui/Spinner.vue';
import Banner from '../../ui/Banner.vue';
import Icon from '../../ui/Icon.vue';
import { useDialogFocus } from '../../../composables/useDialogFocus';

const { t } = useI18n();
const api = getMirriWebApi();

const emit = defineEmits<{
  added: [];
  close: [];
}>();

/** Wire protocols the v2 create/import accepts (kap-server providerWireTypeSchema). */
const WIRE_TYPES = ['kimi', 'openai', 'openai_responses', 'anthropic', 'google-genai', 'vertexai'] as const;
/** Capability chips offered in the manual model editor. */
const CAP_PRESETS = ['image_in', 'thinking', 'tool_use'] as const;
const DEFAULT_MAX_CONTEXT = 128000;
/** Mirrors the v2 providerIdSchema: start with a letter/digit, then letters,
 *  digits, "-", "_" and spaces (Unicode letters/digits, like the server). */
const PROVIDER_ID_RE = /^[\p{L}\p{N}][\p{L}\p{N}\-_ ]*$/u;

const dialogRef = ref<HTMLElement | null>(null);
const searchRef = ref<HTMLInputElement | null>(null);
useDialogFocus(dialogRef, searchRef);

// -------------------------------------------------------------------------
// Catalog fetch
// -------------------------------------------------------------------------
const catalogProviders = ref<AppCatalogProvider[]>([]);
const catalogLoading = ref(false);
const catalogUnavailable = ref(false);
const catalogError = ref('');

/** Ids of providers that already exist in config — re-importing them is a
 *  merge (append-only), which the UI calls out explicitly. */
const existingProviderIds = ref<Set<string>>(new Set());

async function loadCatalog(): Promise<void> {
  catalogLoading.value = true;
  catalogUnavailable.value = false;
  catalogError.value = '';
  try {
    catalogProviders.value = await api.listCatalogProviders();
    existingProviderIds.value = new Set((await api.listProviders()).map((p) => p.id));
  } catch (error) {
    catalogUnavailable.value = true;
    catalogError.value = error instanceof Error ? error.message : String(error);
  } finally {
    catalogLoading.value = false;
  }
}
onMounted(() => { void loadCatalog(); });

// -------------------------------------------------------------------------
// Mode: 'catalog' | 'custom'
// -------------------------------------------------------------------------
const mode = ref<'catalog' | 'custom'>('catalog');
const search = ref('');
const selectedId = ref<string | null>(null);

const filteredCatalog = computed<AppCatalogProvider[]>(() => {
  const q = search.value.toLowerCase().trim();
  if (!q) return catalogProviders.value;
  return catalogProviders.value.filter((p) => {
    return (p.id ?? '').toLowerCase().includes(q) || (p.name ?? '').toLowerCase().includes(q);
  });
});

const selectedProvider = computed<AppCatalogProvider | undefined>(() =>
  catalogProviders.value.find((p) => p.id === selectedId.value),
);

/** True when the selected provider already exists in config: submitting is a
 *  merge (append-only), never a rewrite. */
const selectedIsExisting = computed(() =>
  selectedProvider.value !== undefined && existingProviderIds.value.has(selectedProvider.value.id),
);

function selectProvider(id: string): void {
  const provider = catalogProviders.value.find((p) => p.id === id);
  if (provider === undefined || provider.rejected === true) return;
  // Never carry credentials from a previously selected provider into a new
  // one — a hidden baseUrl/API key would silently configure the wrong
  // endpoint (the base-URL field hides for providers that need none).
  baseUrl.value = '';
  apiKey.value = '';
  selectedId.value = id;
}

// -------------------------------------------------------------------------
// Shared credential fields
// -------------------------------------------------------------------------
const apiKey = ref('');
const baseUrl = ref('');

/** When the input is a pure env-var reference (e.g. `${MY_KEY}`), show it as
 *  plain text — it's not a secret, and seeing the variable name helps. */
const apiKeyIsEnvRef = computed(() =>
  /^\$\{(?:env:)?[A-Za-z_][A-Za-z0-9_]*\}$/.test(apiKey.value.trim()),
);
const apiKeyInputType = computed(() => (apiKeyIsEnvRef.value ? 'text' : 'password'));

/** A provider keyed by an env var (env_key) needs no API key stored here. */
const selectedNeedsApiKey = computed(() => {
  const provider = selectedProvider.value;
  return provider === undefined ? true : !provider.envKey;
});

const submitting = ref(false);
const submitError = ref('');

function switchMode(m: 'catalog' | 'custom'): void {
  mode.value = m;
  selectedId.value = null;
  apiKey.value = '';
  baseUrl.value = '';
  submitError.value = '';
}

// -------------------------------------------------------------------------
// Custom-mode form
// -------------------------------------------------------------------------
const providerId = ref('');
const providerType = ref('openai');

interface ModelRow {
  model: string;
  name: string;
  maxContextSize: string;
  caps: string[];
}

function newModelRow(): ModelRow {
  return { model: '', name: '', maxContextSize: String(DEFAULT_MAX_CONTEXT), caps: [] };
}

const modelRows = ref<ModelRow[]>([newModelRow()]);

function addModelRow(): void {
  modelRows.value.push(newModelRow());
}

function removeModelRow(index: number): void {
  if (modelRows.value.length <= 1) return;
  modelRows.value.splice(index, 1);
}

function toggleCap(row: ModelRow, cap: string): void {
  const idx = row.caps.indexOf(cap);
  if (idx >= 0) row.caps.splice(idx, 1);
  else row.caps.push(cap);
}

function buildCustomModels(): AppAddProviderModelInput[] {
  return modelRows.value.map((row) => ({
    model: row.model.trim(),
    maxContextSize: Number(row.maxContextSize),
    displayName: row.name.trim() || undefined,
    capabilities: row.caps.length > 0 ? [...row.caps] : undefined,
  }));
}

// -------------------------------------------------------------------------
// Submit
// -------------------------------------------------------------------------
async function submit(): Promise<void> {
  submitError.value = '';
  if (mode.value === 'catalog') {
    await submitCatalog();
  } else {
    await submitCustom();
  }
}

async function submitCatalog(): Promise<void> {
  const provider = selectedProvider.value;
  if (provider === undefined) {
    submitError.value = t('settings.models.errSelectProvider');
    return;
  }
  if (provider.needsBaseUrl === true && !baseUrl.value.trim()) {
    submitError.value = t('settings.models.errBaseUrlRequired');
    return;
  }
  if (selectedNeedsApiKey.value && !apiKey.value.trim()) {
    submitError.value = t('settings.models.errApiKey');
    return;
  }
  submitting.value = true;
  try {
    await api.importCatalogProvider({
      catalogId: provider.id,
      apiKey: apiKey.value.trim() || undefined,
      baseUrl: baseUrl.value.trim() || undefined,
    });
    emit('added');
  } catch (error) {
    submitError.value = error instanceof Error ? error.message : String(error);
  } finally {
    submitting.value = false;
  }
}

async function submitCustom(): Promise<void> {
  const id = providerId.value.trim();
  if (!id) {
    submitError.value = t('settings.models.errProviderId');
    return;
  }
  if (!PROVIDER_ID_RE.test(id)) {
    submitError.value = t('settings.models.errProviderIdFormat');
    return;
  }
  // Client-side guard so the user gets a friendly message instead of the
  // server's raw English error on a duplicate id.
  if (existingProviderIds.value.has(id)) {
    submitError.value = t('settings.models.errProviderIdExists');
    return;
  }
  if (!providerType.value) {
    submitError.value = t('settings.models.errProviderType');
    return;
  }
  if (!apiKey.value.trim()) {
    submitError.value = t('settings.models.errApiKey');
    return;
  }
  const models = buildCustomModels();
  if (models.length === 0) {
    submitError.value = t('settings.models.errAtLeastOneModel');
    return;
  }
  const seen = new Set<string>();
  for (const model of models) {
    if (!model.model) {
      submitError.value = t('settings.models.errModelId');
      return;
    }
    if (seen.has(model.model)) {
      submitError.value = t('settings.models.errDuplicateModelId');
      return;
    }
    seen.add(model.model);
    if (!Number.isInteger(model.maxContextSize) || model.maxContextSize <= 0) {
      submitError.value = t('settings.models.errMaxContextInteger');
      return;
    }
  }
  submitting.value = true;
  try {
    await api.addProvider({
      id,
      type: providerType.value,
      apiKey: apiKey.value.trim(),
      baseUrl: baseUrl.value.trim() || undefined,
      models,
    });
    emit('added');
  } catch (error) {
    submitError.value = error instanceof Error ? error.message : String(error);
  } finally {
    submitting.value = false;
  }
}

function onKeydown(e: KeyboardEvent): void {
  if (e.key === 'Escape') emit('close');
}
</script>

<template>
  <Dialog :open="true" :close-on-esc="false" :title="t('settings.models.addProviderTitle')" size="lg" @close="emit('close')" @keydown="onKeydown">
    <div ref="dialogRef" class="apd">
      <!-- Mode switch -->
      <div class="apd-mode">
        <Button :variant="mode === 'catalog' ? 'secondary' : 'ghost'" size="sm" @click="switchMode('catalog')">{{ t('settings.models.modeCatalog') }}</Button>
        <Button :variant="mode === 'custom' ? 'secondary' : 'ghost'" size="sm" @click="switchMode('custom')">{{ t('settings.models.modeCustom') }}</Button>
      </div>

      <!-- Catalog mode -->
      <div v-if="mode === 'catalog'">
        <Banner v-if="catalogUnavailable" variant="warning">{{ t('settings.models.catalogUnavailable') }}</Banner>
        <div v-if="catalogLoading" class="apd-state"><Spinner size="sm" /><span>{{ t('settings.models.catalogLoading') }}</span></div>
        <template v-else-if="!catalogUnavailable">
          <Field :label="t('settings.models.fieldPickProvider')">
            <Input ref="searchRef" v-model="search" :placeholder="t('settings.models.searchProviders')" autocomplete="off" spellcheck="false" />
          </Field>
          <div class="apd-list">
            <button
              v-for="p in filteredCatalog"
              :key="p.id"
              type="button"
              class="apd-prov"
              :class="{ on: p.id === selectedId, rejected: p.rejected === true }"
              :disabled="p.rejected === true"
              :title="p.rejected === true && p.rejectReason ? t('settings.models.catalogRejected', { reason: p.rejectReason }) : undefined"
              @click="selectProvider(p.id)"
            >
              <div class="apd-prov-main">
                <span class="apd-prov-id">{{ p.id }}</span>
                <span v-if="p.name" class="apd-prov-name">{{ p.name }}</span>
              </div>
              <div class="apd-prov-meta">
                <Badge v-if="p.wireType" variant="neutral" size="sm">{{ p.wireType }}</Badge>
                <Badge v-if="existingProviderIds.has(p.id)" variant="info" size="sm">{{ t('settings.models.alreadyAdded') }}</Badge>
                <span v-if="p.rejected === true && p.rejectReason" class="apd-prov-rejected">{{ p.rejectReason }}</span>
                <span class="apd-prov-count">{{ p.models.length }} {{ t('settings.models.modelsUnit') }}</span>
              </div>
            </button>
            <div v-if="filteredCatalog.length === 0" class="apd-empty">{{ t('settings.models.noCatalogMatch') }}</div>
          </div>
        </template>
      </div>

      <!-- Custom mode -->
      <div v-else class="apd-custom">
        <div class="apd-row2">
          <Field :label="t('settings.models.fieldProviderId')" :hint="t('settings.models.fieldProviderIdHint')">
            <Input v-model="providerId" placeholder="my-provider" autocomplete="off" spellcheck="false" @keydown="onKeydown" />
          </Field>
          <Field :label="t('settings.models.fieldProviderType')" :hint="t('settings.models.fieldProviderTypeHint')">
            <Select v-model="providerType">
              <option v-for="wire in WIRE_TYPES" :key="wire" :value="wire">{{ wire }}</option>
            </Select>
          </Field>
        </div>
        <Field :label="t('settings.models.fieldBaseUrl')">
          <Input v-model="baseUrl" :placeholder="t('settings.models.fieldBaseUrlPlaceholder')" @keydown="onKeydown" />
        </Field>

        <!-- Models editor: v2 create requires ≥1 model. -->
        <div class="apd-models">
          <div class="apd-models-head">
            <span class="apd-models-title">{{ t('settings.models.modelsEditorTitle') }}</span>
            <Button variant="ghost" size="sm" @click="addModelRow">
              <Icon name="plus" size="sm" />{{ t('settings.models.addModelRow') }}
            </Button>
          </div>
          <div v-for="(row, index) in modelRows" :key="index" class="apd-model-row">
            <div class="apd-row2">
              <Field :label="t('settings.models.fieldModelId')">
                <Input v-model="row.model" placeholder="gpt-5.2" autocomplete="off" spellcheck="false" />
              </Field>
              <Field :label="t('settings.models.fieldModelName')">
                <Input v-model="row.name" :placeholder="t('settings.models.fieldOptional')" autocomplete="off" spellcheck="false" />
              </Field>
            </div>
            <div class="apd-row2">
              <Field :label="t('settings.models.fieldMaxContext')">
                <Input v-model="row.maxContextSize" type="number" :placeholder="'128000'" />
              </Field>
              <Field :label="t('settings.models.fieldCapabilities')">
                <div class="chip-row">
                  <button
                    v-for="cap in CAP_PRESETS"
                    :key="cap"
                    type="button"
                    class="chip"
                    :class="{ on: row.caps.includes(cap) }"
                    @click="toggleCap(row, cap)"
                  >{{ cap }}</button>
                </div>
              </Field>
            </div>
            <div class="apd-model-actions">
              <Button variant="ghost" size="sm" :disabled="modelRows.length <= 1" @click="removeModelRow(index)">
                <Icon name="close" size="sm" />{{ t('settings.models.removeModelRow') }}
              </Button>
            </div>
          </div>
        </div>
      </div>

      <!-- Shared credential fields (catalog selection or custom mode) -->
      <div v-if="mode === 'custom' || selectedProvider" class="apd-cred">
        <Banner v-if="selectedIsExisting" variant="info">{{ t('settings.models.catalogReimportHint') }}</Banner>
        <div v-if="mode === 'catalog' && selectedProvider" class="apd-row2">
          <Field :label="t('settings.models.fieldResolvedType')" :hint="t('settings.models.fieldResolvedTypeHint')">
            <Input :model-value="selectedProvider.wireType ?? '—'" readonly />
          </Field>
          <Field v-if="selectedProvider.needsBaseUrl === true" :label="t('settings.models.fieldBaseUrl')" :hint="t('settings.models.needsBaseUrlHint')">
            <Input v-model="baseUrl" :placeholder="t('settings.models.fieldBaseUrlPlaceholder')" />
          </Field>
        </div>
        <div v-else-if="mode === 'catalog' && selectedProvider && baseUrl" class="apd-row2">
          <Field :label="t('settings.models.fieldResolvedType')" :hint="t('settings.models.fieldResolvedTypeHint')">
            <Input :model-value="selectedProvider.wireType ?? '—'" readonly />
          </Field>
        </div>
        <Field v-if="mode === 'catalog' && selectedProvider && selectedProvider.envKey" :label="t('settings.models.fieldApiKey')" :hint="t('settings.models.catalogEnvKeyHint', { env: selectedProvider.envKey })">
          <Input v-model="apiKey" :type="apiKeyInputType" placeholder="sk-… or ${MY_VAR}" autocomplete="off" spellcheck="false" @keydown.enter.prevent="submit" />
        </Field>
        <Field v-else :label="t('settings.models.fieldApiKey')" :hint="apiKeyIsEnvRef ? t('settings.models.apiKeyEnvRefHint') : t('settings.models.fieldApiKeyHint')">
          <Input v-model="apiKey" :type="apiKeyInputType" placeholder="sk-… or ${MY_VAR}" autocomplete="off" spellcheck="false" @keydown.enter.prevent="submit" />
        </Field>
        <div v-if="mode === 'catalog' && selectedProvider" class="apd-preview">
          <span class="apd-preview-label">{{ t('settings.models.catalogPreview') }}</span>
          <div class="apd-preview-models">
            <Badge v-for="m in selectedProvider.models.slice(0, 8)" :key="m.id" variant="neutral" size="sm">{{ m.id }}</Badge>
            <span v-if="selectedProvider.models.length > 8" class="apd-preview-more">+{{ selectedProvider.models.length - 8 }}</span>
          </div>
        </div>
      </div>

      <div v-if="submitError" class="apd-error"><Icon name="alert-triangle" size="sm" /><span>{{ submitError }}</span></div>

      <div class="apd-foot">
        <Button
          variant="primary"
          size="sm"
          :loading="submitting"
          :disabled="mode === 'catalog' && !selectedId"
          @click="submit"
        >{{ mode === 'catalog' ? t('settings.models.importCatalog') : t('settings.models.addProvider') }}</Button>
        <Button variant="secondary" size="sm" @click="emit('close')">{{ t('common.cancel') }}</Button>
      </div>
    </div>
  </Dialog>
</template>

<style scoped>
.apd { display: flex; flex-direction: column; gap: var(--space-3); }
.apd-mode { display: flex; gap: var(--space-1); }
.apd-state { display: flex; align-items: center; gap: var(--space-2); color: var(--color-text-muted); font-size: var(--text-sm); padding: var(--space-3) 0; }
.apd-list { max-height: 280px; overflow-y: auto; display: flex; flex-direction: column; gap: 2px; border: 1px solid var(--color-line); border-radius: var(--radius-md); padding: var(--space-1); }
.apd-prov {
  display: flex; align-items: center; justify-content: space-between; gap: var(--space-2);
  text-align: left; padding: var(--space-2) var(--space-3);
  border: none; border-radius: var(--radius-sm); background: transparent; cursor: pointer;
  transition: background var(--duration-fast) var(--ease-out);
}
.apd-prov:hover:not(:disabled) { background: var(--color-surface-sunken); }
.apd-prov.on { background: var(--color-accent-soft); box-shadow: inset 0 0 0 1px var(--color-accent-bd); }
.apd-prov.rejected { opacity: 0.55; cursor: not-allowed; }
.apd-prov-main { display: flex; flex-direction: column; gap: 1px; min-width: 0; }
.apd-prov-id { font-family: var(--font-mono); font-size: var(--text-sm); color: var(--color-text); font-weight: var(--weight-medium); }
.apd-prov-name { font-size: var(--text-xs); color: var(--color-text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.apd-prov-meta { display: flex; align-items: center; gap: var(--space-2); flex: none; }
.apd-prov-rejected { font-size: var(--text-xs); color: var(--color-text-faint); }
.apd-prov-count { font-size: var(--text-xs); color: var(--color-text-faint); }
.apd-empty { padding: var(--space-4); text-align: center; color: var(--color-text-faint); font-size: var(--text-sm); }
.apd-custom { display: flex; flex-direction: column; gap: var(--space-3); }
.apd-row2 { display: grid; grid-template-columns: 1fr 1fr; gap: var(--space-3); }
.apd-cred { display: flex; flex-direction: column; gap: var(--space-3); padding-top: var(--space-2); border-top: 1px solid var(--color-line); }
.apd-preview { display: flex; flex-direction: column; gap: var(--space-1); }
.apd-preview-label { font-size: var(--text-xs); color: var(--color-text-faint); }
.apd-preview-models { display: flex; flex-wrap: wrap; gap: 4px; }
.apd-preview-more { font-size: var(--text-xs); color: var(--color-text-muted); align-self: center; }
.apd-models { display: flex; flex-direction: column; gap: var(--space-2); border: 1px solid var(--color-line); border-radius: var(--radius-md); padding: var(--space-3); }
.apd-models-head { display: flex; align-items: center; justify-content: space-between; gap: var(--space-2); }
.apd-models-title { font-size: var(--text-sm); font-weight: var(--weight-medium); color: var(--color-text); }
.apd-model-row { display: flex; flex-direction: column; gap: var(--space-2); padding: var(--space-2) 0; border-top: 1px solid var(--color-line); }
.apd-model-row:first-of-type { border-top: none; }
.apd-model-actions { display: flex; justify-content: flex-end; }
.chip-row { display: flex; flex-wrap: wrap; gap: 6px; padding-top: 2px; }
.chip {
  font-family: var(--font-mono); font-size: var(--text-xs);
  padding: 4px 8px; border-radius: var(--radius-sm);
  border: 1px solid var(--color-line); background: var(--color-surface-raised);
  color: var(--color-text-muted); cursor: pointer;
  transition: all var(--duration-fast) var(--ease-out);
}
.chip:hover { border-color: var(--color-line-strong); }
.chip.on { background: var(--color-accent-soft); border-color: var(--color-accent-bd); color: var(--color-accent); }
.apd-error { display: flex; align-items: center; gap: var(--space-2); color: var(--color-danger); font-size: var(--text-sm); }
.apd-foot { display: flex; gap: var(--space-2); padding-top: var(--space-2); border-top: 1px solid var(--color-line); }

@media (max-width: 640px) {
  .apd-row2 { grid-template-columns: 1fr; }
}
</style>
