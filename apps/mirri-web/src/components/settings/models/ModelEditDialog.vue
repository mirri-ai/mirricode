<!-- apps/mirri-web/src/components/settings/models/ModelEditDialog.vue -->
<!-- Modal wrapper around ModelEditForm. Rendered above the fullscreen settings
     overlay via Dialog's Teleport + a raised z-index overlay class. Flushes
     pending debounced saves on close so the user never loses an edit. -->
<script setup lang="ts">
import { ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppModelAlias } from '../../../api/types';
import Dialog from '../../ui/Dialog.vue';
import Button from '../../ui/Button.vue';
import Badge from '../../ui/Badge.vue';
import ModelEditForm from './ModelEditForm.vue';

const { t } = useI18n();

const props = defineProps<{
  modelId: string;
  alias: AppModelAlias;
  isDefault?: boolean;
}>();

const emit = defineEmits<{
  close: [];
  saved: [modelId: string, patch: Partial<AppModelAlias>];
  saveError: [modelId: string, error: unknown];
  setDefault: [modelId: string];
}>();

const formRef = ref<InstanceType<typeof ModelEditForm> | null>(null);

function onClose(): void {
  // Flush any pending debounced save so the user's last edit isn't lost.
  formRef.value?.flushNow?.();
  emit('close');
}

function onSaved(modelId: string, patch: Partial<AppModelAlias>): void {
  emit('saved', modelId, patch);
}
</script>

<template>
  <Dialog
    :open="true"
    :close-on-overlay="true"
    :close-on-esc="true"
    size="xl"
    height="fixed"
    overlay-class="model-edit-overlay"
    @close="onClose"
  >
    <template #head>
      <div class="med-head">
        <Badge variant="solid" size="sm" class="med-alias">{{ modelId }}</Badge>
        <span v-if="isDefault" class="med-default">★ {{ t('settings.models.isDefaultModel') }}</span>
      </div>
    </template>

    <ModelEditForm
      ref="formRef"
      :model-id="modelId"
      :alias="alias"
      @saved="onSaved"
      @save-error="(id, err) => emit('saveError', id, err)"
    />

    <template #foot>
      <Button
        variant="secondary"
        size="sm"
        :disabled="isDefault"
        @click="emit('setDefault', modelId)"
      >
        {{ isDefault ? t('settings.models.isDefaultModel') : t('settings.models.setAsDefault') }}
      </Button>
      <Button variant="primary" size="sm" @click="onClose">{{ t('common.done') }}</Button>
    </template>
  </Dialog>
</template>

<style scoped>
.med-head { display: flex; align-items: center; gap: var(--space-2); }
.med-alias { font-family: var(--font-mono); font-size: var(--text-xs); max-width: 280px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.med-default { font-size: var(--text-xs); color: var(--color-accent); }
</style>

<style>
/* Raised z-index so this dialog sits above the FullscreenOverlay (z-modal=400).
   Uses z-modal-confirm (500) — same tier as confirm dialogs. */
.model-edit-overlay { z-index: var(--z-modal-confirm) !important; }
</style>
