<!-- apps/mirri-web/src/components/settings/models/AddProviderDialog.vue -->
<!-- Add-provider dialog. Two paths:
     1. Catalog path — browse models.dev providers (server-proxied), pick one,
        fill API key; submitting POSTs /providers. The server's addProvider
        records the provider; catalog model import happens via refresh.
     2. Custom path — manual type/baseUrl/apiKey for non-catalog providers. -->
<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppCatalogProvider } from '../../../api/types';
import { getMirriWebApi } from '../../../api';
import Dialog from '../../ui/Dialog.vue';
import Button from '../../ui/Button.vue';
import Input from '../../ui/Input.vue';
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

async function loadCatalog(): Promise<void> {
  catalogLoading.value = true;
  catalogUnavailable.value = false;
  catalogError.value = '';
  try {
    catalogProviders.value = await api.listCatalogProviders();
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

function selectProvider(id: string): void {
  selectedId.value = id;
}

// -------------------------------------------------------------------------
// Form fields
// -------------------------------------------------------------------------
const apiKey = ref('');
const baseUrl = ref('');
const providerType = ref('');
const submitting = ref(false);
const submitError = ref('');

/** When the input is a pure env-var reference (e.g. `${MY_KEY}`), show it as
 *  plain text — it's not a secret, and seeing the variable name helps. */
const apiKeyIsEnvRef = computed(() =>
  /^\$\{(?:env:)?[A-Za-z_][A-Za-z0-9_]*\}$/.test(apiKey.value.trim()),
);
const apiKeyInputType = computed(() => (apiKeyIsEnvRef.value ? 'text' : 'password'));

function switchMode(m: 'catalog' | 'custom'): void {
  mode.value = m;
  selectedId.value = null;
  apiKey.value = '';
  baseUrl.value = '';
  providerType.value = '';
  submitError.value = '';
}

// Auto-fill baseUrl + type when a catalog provider is picked.
function onSelectionChange(): void {
  const p = selectedProvider.value;
  if (p !== undefined) {
    baseUrl.value = p.api ?? '';
    providerType.value = p.wire ?? p.type ?? '';
  }
}
watch(selectedId, () => onSelectionChange());

async function submit(): Promise<void> {
  submitError.value = '';
  if (mode.value === 'catalog' && selectedId.value === null) {
    submitError.value = t('settings.models.errSelectProvider');
    return;
  }
  const type = mode.value === 'catalog' ? (selectedProvider.value?.id ?? '') : providerType.value.trim();
  if (!type) {
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
      type,
      apiKey: apiKey.value.trim(),
      baseUrl: baseUrl.value.trim() || undefined,
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
              :class="{ on: p.id === selectedId }"
              @click="selectProvider(p.id)"
            >
              <div class="apd-prov-main">
                <span class="apd-prov-id">{{ p.id }}</span>
                <span v-if="p.name" class="apd-prov-name">{{ p.name }}</span>
              </div>
              <div class="apd-prov-meta">
                <Badge v-if="p.wire" variant="neutral" size="sm">{{ p.wire }}</Badge>
                <span class="apd-prov-count">{{ p.models.length }} {{ t('settings.models.modelsUnit') }}</span>
              </div>
            </button>
            <div v-if="filteredCatalog.length === 0" class="apd-empty">{{ t('settings.models.noCatalogMatch') }}</div>
          </div>
        </template>
      </div>

      <!-- Custom mode -->
      <div v-else class="apd-custom">
        <Field :label="t('settings.models.fieldProviderType')" :hint="t('settings.models.fieldProviderTypeHint')">
          <Input v-model="providerType" :placeholder="'moonshot / anthropic / openai'" @keydown="onKeydown" />
        </Field>
        <Field :label="t('settings.models.fieldBaseUrl')">
          <Input v-model="baseUrl" :placeholder="t('settings.models.fieldBaseUrlPlaceholder')" @keydown="onKeydown" />
        </Field>
      </div>

      <!-- Shared credential fields (shown once a provider is chosen or in custom mode) -->
      <div v-if="(mode === 'catalog' && selectedProvider) || mode === 'custom'" class="apd-cred">
        <Field v-if="mode === 'catalog'" :label="t('settings.models.fieldResolvedType')" :hint="t('settings.models.fieldResolvedTypeHint')">
          <Input :model-value="providerType" readonly />
        </Field>
        <Field v-if="baseUrl" :label="t('settings.models.fieldBaseUrl')">
          <Input v-model="baseUrl" :placeholder="t('settings.models.fieldBaseUrlPlaceholder')" />
        </Field>
        <Field :label="t('settings.models.fieldApiKey')" :hint="apiKeyIsEnvRef ? t('settings.models.apiKeyEnvRefHint') : t('settings.models.fieldApiKeyHint')">
          <Input v-model="apiKey" :type="apiKeyInputType" placeholder="sk-… or ${MY_VAR}" autocomplete="off" spellcheck="false" @keydown.enter.prevent="submit" />
        </Field>
        <div v-if="selectedProvider" class="apd-preview">
          <span class="apd-preview-label">{{ t('settings.models.catalogPreview') }}</span>
          <div class="apd-preview-models">
            <Badge v-for="m in selectedProvider.models.slice(0, 8)" :key="m.id" variant="neutral" size="sm">{{ m.id }}</Badge>
            <span v-if="selectedProvider.models.length > 8" class="apd-preview-more">+{{ selectedProvider.models.length - 8 }}</span>
          </div>
        </div>
      </div>

      <div v-if="submitError" class="apd-error"><Icon name="alert-triangle" size="sm" /><span>{{ submitError }}</span></div>

      <div class="apd-foot">
        <Button variant="primary" size="sm" :loading="submitting" :disabled="mode === 'catalog' && !selectedId" @click="submit">{{ t('settings.models.addProvider') }}</Button>
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
.apd-prov:hover { background: var(--color-surface-sunken); }
.apd-prov.on { background: var(--color-accent-soft); box-shadow: inset 0 0 0 1px var(--color-accent-bd); }
.apd-prov-main { display: flex; flex-direction: column; gap: 1px; min-width: 0; }
.apd-prov-id { font-family: var(--font-mono); font-size: var(--text-sm); color: var(--color-text); font-weight: var(--weight-medium); }
.apd-prov-name { font-size: var(--text-xs); color: var(--color-text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.apd-prov-meta { display: flex; align-items: center; gap: var(--space-2); flex: none; }
.apd-prov-count { font-size: var(--text-xs); color: var(--color-text-faint); }
.apd-empty { padding: var(--space-4); text-align: center; color: var(--color-text-faint); font-size: var(--text-sm); }
.apd-cred { display: flex; flex-direction: column; gap: var(--space-3); padding-top: var(--space-2); border-top: 1px solid var(--color-line); }
.apd-preview { display: flex; flex-direction: column; gap: var(--space-1); }
.apd-preview-label { font-size: var(--text-xs); color: var(--color-text-faint); }
.apd-preview-models { display: flex; flex-wrap: wrap; gap: 4px; }
.apd-preview-more { font-size: var(--text-xs); color: var(--color-text-muted); align-self: center; }
.apd-error { display: flex; align-items: center; gap: var(--space-2); color: var(--color-danger); font-size: var(--text-sm); }
.apd-foot { display: flex; gap: var(--space-2); padding-top: var(--space-2); border-top: 1px solid var(--color-line); }
</style>
