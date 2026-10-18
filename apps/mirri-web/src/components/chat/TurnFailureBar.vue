<!-- apps/mirri-web/src/components/chat/TurnFailureBar.vue -->
<!-- Bottom one-line failure bar for the PREVIOUS turn's error (user-ref:
     litellm-style quiet strip above the composer, NOT a red toast). Neutral
     info styling: the transcript keeps flowing and the product does not look
     broken. Show details opens a dialog with the FULL message and
     diagnostics (the strip's single line is always truncated); Copy copies
     the full notice; no auto-dismiss — a new turn or the × clears it. -->
<script lang="ts">
import { i18n } from '../../i18n';
import type { AppNotice } from '../../api/types';

/** One-line summary: `Title: message` (title alone when no message). */
export function turnFailureSummary(notice: AppNotice): string {
  return notice.message ? `${notice.title}: ${notice.message}` : notice.title;
}

/** Full multi-line text for the clipboard copy. */
export function turnFailureCopyText(notice: AppNotice): string {
  const lines = [notice.title];
  if (notice.message) lines.push(notice.message);
  const details = notice.details ?? [];
  if (details.length > 0) {
    lines.push('', `${i18n.global.t('warnings.diagnostics')}:`);
    for (const detail of details) lines.push(`${detail.label}: ${detail.value}`);
  }
  return lines.join('\n');
}
</script>

<script setup lang="ts">
import { computed, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import Icon from '../ui/Icon.vue';
import IconButton from '../ui/IconButton.vue';
import Dialog from '../ui/Dialog.vue';
import { copyTextToClipboard } from '../../lib/clipboard';
import type { AppWarning } from '../../api/types';

const props = defineProps<{ warnings: AppWarning[] }>();
const emit = defineEmits<{ dismiss: [index: number] }>();

const { t } = useI18n();
const detailsOpen = ref(false);
const copied = ref(false);
let copiedTimer: ReturnType<typeof setTimeout> | undefined;

/** The single turn-failure notice the bar renders (at most one by the
 *  reducer's new-turn clearing). */
const notice = computed<AppNotice | undefined>(() => {
  const found = props.warnings.find(
    (w): w is AppNotice => typeof w === 'object' && w !== null && w.channel === 'turn-failure',
  );
  return found;
});

function onCopy(): void {
  if (notice.value === undefined) return;
  void copyTextToClipboard(turnFailureCopyText(notice.value)).then((ok) => {
    if (!ok) return;
    copied.value = true;
    if (copiedTimer !== undefined) clearTimeout(copiedTimer);
    copiedTimer = setTimeout(() => {
      copied.value = false;
    }, 1400);
  });
}
</script>

<template>
  <div v-if="notice !== undefined" class="tf-bar" role="status" aria-live="polite">
    <span class="tf-icon" aria-hidden="true"><Icon name="info" size="sm" /></span>
    <span class="tf-text" :title="turnFailureSummary(notice)">{{ turnFailureSummary(notice) }}</span>
    <button class="tf-link" type="button" @click="detailsOpen = true">
      {{ t('warnings.showDetails') }}
    </button>
    <button class="tf-link" type="button" @click="onCopy">
      {{ copied ? t('warnings.copied') : t('warnings.copyDetails') }}
    </button>
    <IconButton class="tf-close" size="sm" :label="t('warnings.dismiss')" @click="emit('dismiss', 0)">
      <Icon name="close" size="sm" />
    </IconButton>

    <!-- Full-content dialog: the strip's one line is always truncated — the
         dialog is where the whole message and diagnostics become readable. -->
    <Dialog
      :open="detailsOpen"
      size="lg"
      :title="notice.title"
      @update:open="detailsOpen = $event"
    >
      <div v-if="notice.message" class="tf-dialog-message">{{ notice.message }}</div>
      <dl v-if="(notice.details?.length ?? 0) > 0" class="tf-dialog-details">
        <div v-for="detail in notice.details" :key="`${detail.label}:${detail.value}`" class="tf-dialog-row">
          <dt>{{ detail.label }}</dt>
          <dd>{{ detail.value }}</dd>
        </div>
      </dl>
      <template #foot>
        <button class="tf-link" type="button" @click="onCopy">
          {{ copied ? t('warnings.copied') : t('warnings.copyDetails') }}
        </button>
      </template>
    </Dialog>
  </div>
</template>

<style scoped>
.tf-bar {
  box-sizing: border-box;
  display: flex;
  align-items: center;
  gap: 8px;
  margin: 0 0 6px;
  padding: 6px 12px;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-sm);
  background: var(--color-surface-raised);
  font-size: var(--text-sm);
}
.tf-icon {
  flex: none;
  color: var(--color-text-muted);
  display: grid;
  place-items: center;
}
.tf-text {
  flex: 1;
  min-width: 0;
  color: var(--color-text-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.tf-link {
  flex: none;
  border: 0;
  padding: 0;
  background: none;
  color: var(--color-accent);
  cursor: pointer;
  font: inherit;
  font-size: var(--ui-font-size-xs);
}
.tf-link:hover { text-decoration: underline; }
.tf-close { flex: none; margin: -3px -4px 0 0; }
.tf-dialog-message {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  color: var(--color-text);
  font-size: var(--text-sm);
  line-height: 1.5;
}
.tf-dialog-details {
  display: grid;
  gap: 5px;
  margin: 12px 0 0;
  padding: 10px;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-sm);
  background: var(--color-surface-sunken);
}
.tf-dialog-row {
  display: grid;
  grid-template-columns: minmax(96px, 0.3fr) minmax(0, 1fr);
  gap: 8px;
}
.tf-dialog-row dt { color: var(--color-text-muted); }
.tf-dialog-row dd {
  margin: 0;
  color: var(--color-text);
  overflow-wrap: anywhere;
  white-space: pre-wrap;
}
</style>
