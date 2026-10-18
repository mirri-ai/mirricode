<!-- apps/mirri-web/src/components/dialogs/DeleteWorkspaceDialog.vue -->
<!-- Design-system §03 modal confirmation, W6 three-choice shape: the
     primary DANGER button purges (deletes the data and unregisters — Enter
     default via initial-focus), the secondary button only removes from the
     list (every byte preserved; re-adding the same root restores the
     sessions), Esc/overlay = cancel. Shows the workspace's session count. -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';

import Dialog from '../ui/Dialog.vue';
import Button from '../ui/Button.vue';
import {
  buildDeleteWorkspaceDialogModel,
  useDeleteWorkspaceChoice,
} from '../../composables/useDeleteWorkspaceChoice';

const { current } = useDeleteWorkspaceChoice();
const { t } = useI18n();

const emit = defineEmits<{
  purge: [];
  remove: [];
  cancel: [];
}>();

const target = computed(() => current.value?.target ?? null);

const model = computed(() =>
  buildDeleteWorkspaceDialogModel(target.value, {
    count: (sessionCount: number) => t('workspace.sessionCount', sessionCount),
    purge: t('workspace.deletePurgeLabel'),
    remove: t('workspace.deleteRemoveLabel'),
    cancel: t('common.cancel'),
    hint: t('workspace.deletePurgeHint'),
  }),
);
</script>

<template>
  <Dialog
    :open="target !== null"
    :title="model.title"
    height="auto"
    initial-focus=".delete-workspace-dialog__purge"
    overlay-class="ui-dialog__overlay--confirm"
    @update:open="(value: boolean) => (!value && emit('cancel'))"
    @close="emit('cancel')"
  >
    <div v-if="target" class="delete-workspace-dialog__body">
      <p class="delete-workspace-dialog__count">
        {{ model.countLine }}
      </p>
      <p class="delete-workspace-dialog__hint">
        {{ model.hint }}
      </p>
    </div>
    <template #foot>
      <Button variant="secondary" @click="emit('cancel')">
        {{ model.cancelLabel }}
      </Button>
      <Button variant="secondary" @click="emit('remove')">
        {{ model.removeLabel }}
      </Button>
      <Button
        class="delete-workspace-dialog__purge"
        variant="danger"
        @click="emit('purge')"
      >
        {{ model.purgeLabel }}
      </Button>
    </template>
  </Dialog>
</template>

<style scoped>
.delete-workspace-dialog__body {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}

.delete-workspace-dialog__count {
  margin: 0;
  font-size: var(--text-base);
  line-height: var(--leading-normal);
  color: var(--color-text-muted);
}

.delete-workspace-dialog__hint {
  margin: 0;
  font-size: var(--text-sm);
  line-height: var(--leading-normal);
  color: var(--color-text-muted);
}
</style>
