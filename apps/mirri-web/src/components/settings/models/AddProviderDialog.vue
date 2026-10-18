<!-- apps/mirri-web/src/components/settings/models/AddProviderDialog.vue -->
<!-- Add-provider dialog, v2-only (kap-server). Two paths:
     1. Catalog path — browse the models.dev catalog (server-proxied), pick one,
        fill API key / base URL; submitting POSTs /providers:import_catalog.
        Importing an id that already exists appends only new catalog models —
        existing aliases (incl. LLM-aware settings) are never rewritten and
        user-deleted models are never resurrected.
     2. Custom path — manual id/type/baseUrl/apiKey for non-catalog providers;
        submitting POSTs /providers (v2 create). The create schema accepts an
        empty models list, so a provider is created without models and models
        are added later from the provider detail panel. -->
<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppCatalogProvider } from '../../../api/types';
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
import { isCompositionKeyEvent } from '../../../lib/keyboard';
import { modelCapabilityBadges } from '../../../lib/modelCapabilities';

const { t } = useI18n();
const api = getMirriWebApi();

const emit = defineEmits<{
  /** A provider was created/imported. Custom-mode creates report the id so
   *  the parent can select the new provider; the catalog import path omits
   *  it (it may merge into an existing provider and always ships models). */
  added: [providerId?: string];
  close: [];
}>();

/** Wire protocols the v2 create/import accepts (kap-server providerWireTypeSchema). */
const WIRE_TYPES = ['kimi', 'openai', 'openai_responses', 'anthropic', 'google-genai', 'vertexai'] as const;
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
const catalogRefreshing = ref(false);

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

/** An empty directory (no providers) or an unavailable one is served the
 *  live-catalog refresh action: the engine pulls the models.dev directory
 *  (the bundled bootstrap snapshot is only a fallback) and then the browse
 *  is re-run. */
async function refreshCatalog(): Promise<void> {
  if (catalogRefreshing.value) return;
  catalogRefreshing.value = true;
  try {
    await api.refreshAllProviders();
    await loadCatalog();
  } catch (error) {
    catalogUnavailable.value = true;
    catalogError.value = error instanceof Error ? error.message : String(error);
  } finally {
    catalogRefreshing.value = false;
  }
}

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
  submitting.value = true;
  try {
    await api.addProvider({
      id,
      type: providerType.value,
      apiKey: apiKey.value.trim(),
      baseUrl: baseUrl.value.trim() || undefined,
      // The v2 create schema requires the models key but allows an empty
      // list: a custom provider is created without models and models are
      // added later from the provider detail panel.
      models: [],
    });
    // Report the created id so the parent can select the new provider and
    // guide the user to add its first model (custom creates have none).
    emit('added', id);
  } catch (error) {
    submitError.value = error instanceof Error ? error.message : String(error);
  } finally {
    submitting.value = false;
  }
}

function onApiKeyKeydown(e: KeyboardEvent): void {
  if (isCompositionKeyEvent(e)) return;
  if (e.key === 'Enter') {
    e.preventDefault();
    submit();
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
        <div v-if="catalogUnavailable || (!catalogLoading && catalogProviders.length === 0)" class="apd-catalog-empty">
          <Banner :variant="catalogUnavailable ? 'warning' : 'info'">
            {{ catalogUnavailable ? t('settings.models.catalogUnavailable') : t('settings.models.catalogEmpty') }}
          </Banner>
          <Button variant="secondary" size="sm" :loading="catalogRefreshing" @click="refreshCatalog">
            <Icon name="refresh" size="sm" />{{ t('settings.models.catalogRefresh') }}
          </Button>
        </div>
        <div v-if="catalogLoading" class="apd-state"><Spinner size="sm" /><span>{{ t('settings.models.catalogLoading') }}</span></div>
        <template v-else-if="!catalogUnavailable && catalogProviders.length > 0">
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
          <Input v-model="apiKey" :type="apiKeyInputType" placeholder="sk-… or ${MY_VAR}" autocomplete="off" spellcheck="false" @keydown="onApiKeyKeydown" />
        </Field>
        <Field v-else :label="t('settings.models.fieldApiKey')" :hint="apiKeyIsEnvRef ? t('settings.models.apiKeyEnvRefHint') : t('settings.models.fieldApiKeyHint')">
          <Input v-model="apiKey" :type="apiKeyInputType" placeholder="sk-… or ${MY_VAR}" autocomplete="off" spellcheck="false" @keydown="onApiKeyKeydown" />
        </Field>
        <div v-if="mode === 'catalog' && selectedProvider" class="apd-preview">
          <span class="apd-preview-label">{{ t('settings.models.catalogPreview') }}</span>
          <div class="apd-preview-models">
            <Badge
              v-for="m in selectedProvider.models.slice(0, 8)"
              :key="m.id"
              variant="neutral"
              size="sm"
              :title="modelCapabilityBadges(m.capabilities, m.reasoning).map((b) => t(b.label)).join(', ') || undefined"
            >{{ m.id }}<span
              v-for="badge in modelCapabilityBadges(m.capabilities, m.reasoning)"
              :key="badge.key"
              class="apd-cap"
              :class="{ 'apd-cap-disabled': badge.disabled }"
            >·{{ t(badge.label) }}</span></Badge>
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
.apd-catalog-empty { display: flex; flex-direction: column; align-items: flex-start; gap: var(--space-2); padding: var(--space-2) 0; }
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
.apd-cap {
  opacity: 0.55;
  margin-left: 2px;
}
.apd-cap-disabled {
  opacity: 0.35;
}
.apd-error { display: flex; align-items: center; gap: var(--space-2); color: var(--color-danger); font-size: var(--text-sm); }
.apd-foot { display: flex; gap: var(--space-2); padding-top: var(--space-2); border-top: 1px solid var(--color-line); }

@media (max-width: 640px) {
  .apd-row2 { grid-template-columns: 1fr; }
}
</style>
