<!-- apps/mirri-web/src/components/settings/models/ModelsPanel.vue -->
<!-- The "Models" Settings tab. Self-contained: loads providers + models from
     the daemon, offers provider CRUD (catalog + custom), and hosts the
     per-model editor. Two-pane master-detail layout. -->
<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppModelAlias, AppProvider } from '../../../api/types';
import { getMirriWebApi } from '../../../api';
import Button from '../../ui/Button.vue';
import Badge from '../../ui/Badge.vue';
import Spinner from '../../ui/Spinner.vue';
import Icon from '../../ui/Icon.vue';
import Banner from '../../ui/Banner.vue';
import ProviderDetail from './ProviderDetail.vue';
import AddProviderDialog from './AddProviderDialog.vue';

const { t } = useI18n();
const api = getMirriWebApi();

// -------------------------------------------------------------------------
// State
// -------------------------------------------------------------------------
const providers = ref<AppProvider[]>([]);
const models = ref<Record<string, AppModelAlias>>({});
const defaultModel = ref<string | undefined>(undefined);
const loading = ref(false);
const loadError = ref('');
const selectedProviderId = ref<string | null>(null);
const refreshingProviderId = ref<string | null>(null);
const showAddDialog = ref(false);
const refreshBanner = ref<{ added: number; removed: number; unchanged: number; failed: number } | null>(null);

async function load(): Promise<void> {
  loading.value = true;
  loadError.value = '';
  try {
    const [provResult, configResult] = await Promise.all([
      api.listProviders(),
      api.getConfig(),
    ]);
    providers.value = provResult;
    models.value = configResult.models ?? {};
    defaultModel.value = configResult.defaultModel;
    // Keep selection valid.
    if (selectedProviderId.value !== null && !providers.value.some((p) => p.id === selectedProviderId.value)) {
      selectedProviderId.value = providers.value[0]?.id ?? null;
    }
    if (selectedProviderId.value === null && providers.value.length > 0) {
      selectedProviderId.value = providers.value[0]?.id ?? null;
    }
  } catch (error) {
    loadError.value = error instanceof Error ? error.message : String(error);
  } finally {
    loading.value = false;
  }
}
onMounted(() => { void load(); });

const selectedProvider = computed<AppProvider | undefined>(() =>
  providers.value.find((p) => p.id === selectedProviderId.value),
);

function selectProvider(id: string): void {
  selectedProviderId.value = id;
}

// -------------------------------------------------------------------------
// Actions
// -------------------------------------------------------------------------
async function refreshProvider(id: string): Promise<void> {
  refreshingProviderId.value = id;
  try {
    const result = await api.refreshProvider(id);
    showRefreshBanner(result);
    await load();
  } catch (error) {
    console.error('refreshProvider failed', error);
  } finally {
    refreshingProviderId.value = null;
  }
}

async function refreshAll(): Promise<void> {
  refreshingProviderId.value = '__all__';
  try {
    const result = await api.refreshAllProviders();
    showRefreshBanner(result);
    await load();
  } catch (error) {
    console.error('refreshAllProviders failed', error);
  } finally {
    refreshingProviderId.value = null;
  }
}

function showRefreshBanner(result: { changed: { added: number; removed: number }[]; unchanged: string[]; failed: { provider: string; reason: string }[] }): void {
  const added = result.changed.reduce((sum, c) => sum + c.added, 0);
  const removed = result.changed.reduce((sum, c) => sum + c.removed, 0);
  refreshBanner.value = { added, removed, unchanged: result.unchanged.length, failed: result.failed.length };
  setTimeout(() => { refreshBanner.value = null; }, 5000);
}

async function deleteProvider(id: string): Promise<void> {
  try {
    await api.deleteProvider(id);
    if (selectedProviderId.value === id) selectedProviderId.value = null;
    await load();
  } catch (error) {
    console.error('deleteProvider failed', error);
  }
}

async function setDefaultModel(modelId: string): Promise<void> {
  try {
    await api.setConfig({ defaultModel: modelId });
    defaultModel.value = modelId;
  } catch (error) {
    console.error('setDefaultModel failed', error);
  }
}

function onAdded(): void {
  showAddDialog.value = false;
  void load();
}

function onProviderChanged(): void {
  void load();
}

function onModelSaved(modelId: string, patch: Partial<AppModelAlias>): void {
  const existing = models.value[modelId];
  if (!existing) return;
  models.value[modelId] = { ...existing, ...patch };
}

function statusColor(status: AppProvider['status']): string {
  if (status === 'connected') return 'var(--color-success)';
  if (status === 'error') return 'var(--color-danger)';
  return 'var(--color-text-faint)';
}

function modelCountFor(id: string): number {
  return Object.values(models.value).filter((m) => m.provider === id).length;
}
</script>

<template>
  <div class="mp">
    <!-- Toolbar -->
    <div class="mp-toolbar">
      <Button variant="primary" size="sm" @click="showAddDialog = true">
        <Icon name="plus" size="sm" />{{ t('settings.models.addProvider') }}
      </Button>
      <Button variant="secondary" size="sm" :loading="refreshingProviderId === '__all__'" @click="refreshAll">
        <Icon name="undo" size="sm" />{{ t('settings.models.refreshAll') }}
      </Button>
      <span v-if="defaultModel" class="mp-default">
        <Icon name="star" size="sm" />
        <span class="mp-default-id">{{ defaultModel }}</span>
      </span>
    </div>

    <Banner v-if="refreshBanner" variant="info">
      {{ t('settings.models.refreshResult', { added: refreshBanner.added, removed: refreshBanner.removed, unchanged: refreshBanner.unchanged, failed: refreshBanner.failed }) }}
    </Banner>
    <Banner v-if="loadError" variant="danger">{{ loadError }}</Banner>

    <div v-if="loading" class="mp-state"><Spinner size="sm" /><span>{{ t('settings.models.loading') }}</span></div>

    <div v-else class="mp-body">
      <!-- Left: provider list -->
      <nav class="mp-provs">
        <button
          v-for="p in providers"
          :key="p.id"
          type="button"
          class="mp-prov"
          :class="{ on: p.id === selectedProviderId }"
          @click="selectProvider(p.id)"
        >
          <span class="status-dot" :style="{ background: statusColor(p.status) }" />
          <span class="mp-prov-main">
            <span class="mp-prov-id">{{ p.id }}</span>
            <Badge variant="neutral" size="sm" class="mp-prov-type">{{ p.type }}</Badge>
          </span>
          <span class="mp-prov-count">{{ modelCountFor(p.id) }}</span>
        </button>
        <div v-if="providers.length === 0" class="mp-provs-empty">{{ t('settings.models.noProviders') }}</div>
      </nav>

      <!-- Right: detail -->
      <div class="mp-detail">
        <ProviderDetail
          v-if="selectedProvider"
          :provider="selectedProvider"
          :models="models"
          :default-model="defaultModel"
          :refreshing="refreshingProviderId === selectedProvider.id"
          @refresh="refreshProvider"
          @delete="deleteProvider"
          @changed="onProviderChanged"
          @set-default-model="setDefaultModel"
          @saved="onModelSaved"
        />
        <div v-else class="mp-detail-empty">
          <Icon name="info" size="lg" />
          <span>{{ t('settings.models.selectProviderHint') }}</span>
        </div>
      </div>
    </div>

    <AddProviderDialog v-if="showAddDialog" @added="onAdded" @close="showAddDialog = false" />
  </div>
</template>

<style scoped>
.mp { display: flex; flex-direction: column; gap: var(--space-3); height: 100%; min-height: 0; }

.mp-toolbar { display: flex; align-items: center; gap: var(--space-2); flex-wrap: wrap; }
.mp-default { display: inline-flex; align-items: center; gap: 4px; margin-left: auto; font-family: var(--font-mono); font-size: var(--text-xs); color: var(--color-text-muted); overflow: hidden; }
.mp-default-id { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

.mp-state { display: flex; align-items: center; gap: var(--space-2); color: var(--color-text-muted); font-size: var(--text-sm); padding: var(--space-4) 0; }

.mp-body { display: grid; grid-template-columns: 200px 1fr; gap: var(--space-4); min-height: 0; flex: 1; }

.mp-provs { display: flex; flex-direction: column; gap: 1px; overflow-y: auto; min-height: 0; }
.mp-prov {
  display: flex; align-items: center; gap: var(--space-2);
  text-align: left; padding: var(--space-2) var(--space-3);
  border: none; border-radius: var(--radius-md); background: transparent; cursor: pointer;
  transition: background var(--duration-fast) var(--ease-out);
}
.mp-prov:hover { background: var(--color-surface-sunken); }
.mp-prov.on { background: var(--color-accent-soft); color: var(--color-accent); }
.status-dot { width: 8px; height: 8px; border-radius: 50%; flex: none; }
.mp-prov-main { display: flex; flex-direction: column; gap: 2px; min-width: 0; flex: 1; }
.mp-prov-id { font-family: var(--font-ui); font-size: var(--text-sm); font-weight: var(--weight-medium); color: var(--color-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.mp-prov.on .mp-prov-id { color: var(--color-accent); }
.mp-prov-type { align-self: flex-start; }
.mp-prov-count { flex: none; font-size: var(--text-xs); color: var(--color-text-muted); }
.mp-provs-empty { padding: var(--space-4); text-align: center; color: var(--color-text-faint); font-size: var(--text-sm); }

.mp-detail { min-width: 0; overflow-y: auto; }
.mp-detail-empty { display: flex; flex-direction: column; align-items: center; justify-content: center; gap: var(--space-2); height: 100%; color: var(--color-text-faint); font-size: var(--text-sm); }

@media (max-width: 640px) {
  .mp-body { grid-template-columns: 1fr; }
  .mp-provs { flex-direction: row; overflow-x: auto; max-height: none; }
  .mp-prov { flex: none; white-space: nowrap; }
}
</style>
