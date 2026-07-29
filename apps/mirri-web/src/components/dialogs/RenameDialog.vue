<!-- apps/mirri-web/src/components/dialogs/RenameDialog.vue -->
<!-- Modal dialog for renaming a session/task. Shows the current title as the
     default value and lets the user enter a new one before confirming. -->
<script setup lang="ts">
import { nextTick, onBeforeUnmount, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import Dialog from '../ui/Dialog.vue';
import Button from '../ui/Button.vue';
import Input from '../ui/Input.vue';

const props = defineProps<{
  open: boolean;
  currentTitle: string;
}>();

const emit = defineEmits<{
  'update:open': [value: boolean];
  confirm: [newTitle: string];
  cancel: [];
}>();

const { t } = useI18n();
const value = ref('');
const inputRef = ref<InstanceType<typeof Input> | null>(null);

// Sync the input value whenever the dialog opens or the current title changes.
watch(
  () => [props.open, props.currentTitle] as const,
  ([open]) => {
    if (open) {
      value.value = props.currentTitle;
      void nextTick(() => {
        inputRef.value?.focus();
        inputRef.value?.select();
      });
    }
  },
  { immediate: true },
);

const trimmed = () => value.value.trim();
const canConfirm = () => trimmed().length > 0 && trimmed() !== props.currentTitle.trim();

function onConfirm(): void {
  if (!canConfirm()) return;
  emit('confirm', trimmed());
  emit('update:open', false);
}

function onCancel(): void {
  emit('update:open', false);
  emit('cancel');
}

function onKeydown(event: KeyboardEvent): void {
  if (event.key === 'Enter' && props.open) {
    event.preventDefault();
    onConfirm();
  }
}

if (typeof window !== 'undefined') {
  window.addEventListener('keydown', onKeydown);
}

onBeforeUnmount(() => {
  if (typeof window !== 'undefined') window.removeEventListener('keydown', onKeydown);
});
</script>

<template>
  <Dialog
    :open="open"
    :title="t('header.renameDialogTitle')"
    height="auto"
    initial-focus=".rename-dialog__input input"
    @update:open="emit('update:open', $event)"
    @close="onCancel"
  >
    <Input
      ref="inputRef"
      v-model="value"
      class="rename-dialog__input"
      :placeholder="t('header.renameDialogPlaceholder')"
    />
    <template #foot>
      <Button variant="secondary" @click="onCancel">
        {{ t('header.renameDialogCancel') }}
      </Button>
      <Button
        :disabled="!canConfirm()"
        @click="onConfirm"
      >
        {{ t('header.renameDialogConfirm') }}
      </Button>
    </template>
  </Dialog>
</template>
