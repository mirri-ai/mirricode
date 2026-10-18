<!-- apps/mirri-web/src/components/dialogs/TrustWorkspaceDialog.vue -->
<!-- Design-system §03 modal confirmation, the W8 SDR-007 shape: selecting
     an untrusted workspace fires this NON-BLOCKING confirmation — the
     primary button trusts the workspace (its project-level config loads),
     the secondary button is 暂不 (the workspace stays usable, the project
     config stays honestly not-in-effect), Esc/overlay = 暂不. -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';

import Dialog from '../ui/Dialog.vue';
import Button from '../ui/Button.vue';
import {
  buildTrustWorkspaceDialogModel,
  useWorkspaceTrustPrompt,
} from '../../composables/useWorkspaceTrustPrompt';

const { current } = useWorkspaceTrustPrompt();
const { t } = useI18n();

const emit = defineEmits<{
  trust: [];
  later: [];
}>();

const target = computed(() => current.value?.target ?? null);

const model = computed(() =>
  buildTrustWorkspaceDialogModel(target.value, {
    trust: t('workspace.trustConfirmLabel'),
    later: t('workspace.trustLaterLabel'),
    hint: t('workspace.trustHint'),
  }),
);
</script>

<template>
  <Dialog
    :open="target !== null"
    :title="model.title"
    height="auto"
    initial-focus=".trust-workspace-dialog__trust"
    overlay-class="ui-dialog__overlay--confirm"
    @update:open="(value: boolean) => (!value && emit('later'))"
    @close="emit('later')"
  >
    <div v-if="target" class="trust-workspace-dialog__body">
      <p class="trust-workspace-dialog__root">
        {{ model.rootLine }}
      </p>
      <p class="trust-workspace-dialog__hint">
        {{ model.hint }}
      </p>
    </div>
    <template #foot>
      <Button variant="secondary" @click="emit('later')">
        {{ model.laterLabel }}
      </Button>
      <Button
        class="trust-workspace-dialog__trust"
        variant="primary"
        @click="emit('trust')"
      >
        {{ model.trustLabel }}
      </Button>
    </template>
  </Dialog>
</template>

<style scoped>
.trust-workspace-dialog__body {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}

.trust-workspace-dialog__root {
  margin: 0;
  font-size: var(--text-base);
  line-height: var(--leading-normal);
  color: var(--color-text-muted);
  word-break: break-all;
}

.trust-workspace-dialog__hint {
  margin: 0;
  font-size: var(--text-sm);
  line-height: var(--leading-normal);
  color: var(--color-text-muted);
}
</style>
