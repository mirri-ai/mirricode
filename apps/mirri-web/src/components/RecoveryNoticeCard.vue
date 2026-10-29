<!-- apps/mirri-web/src/components/RecoveryNoticeCard.vue -->
<!-- Interrupted-task recovery notice card (Epic C, ADR-004 pull model):
     a non-modal card listing sessions the app abandoned mid-run. Per-row
     resume/dismiss, footer resume-all/dismiss-all, and a close button that
     ONLY dismisses the UI for this frontend run (C.6 — it binds no RPC).
     All actions leave via emits; App.vue routes them to the client actions
     (the WarningToasts props/emits precedent). -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppRecoveryInterruption } from '../api/types';
import {
  pickNoticeTitleKey,
  relativeTimeFromEpoch,
} from '../lib/recoveryNotice';
import Button from './ui/Button.vue';
import Card from './ui/Card.vue';
import IconButton from './ui/IconButton.vue';
import Icon from './ui/Icon.vue';
import Tooltip from './ui/Tooltip.vue';

const props = defineProps<{
  interruptions: AppRecoveryInterruption[];
}>();

const emit = defineEmits<{
  resume: [entry: AppRecoveryInterruption];
  dismiss: [entry: AppRecoveryInterruption];
  'open-session': [entry: AppRecoveryInterruption];
  'resume-all': [];
  'dismiss-all': [];
  close: [];
}>();

const { t } = useI18n();

const noticeTitleKey = computed(() => pickNoticeTitleKey(props.interruptions));

/** Relative times refresh with the same 30s clock the sidebar rows share
 *  (the tick value is voided here so a tick recomputes only the labels). */
const nowTick = defineModel<number | null>('nowTick', { default: null });

function relativeTime(entry: AppRecoveryInterruption): string {
  void nowTick.value;
  return relativeTimeFromEpoch(entry.interruptedAt, Date.now());
}
</script>

<template>
  <Card class="recovery-notice" elevated>
    <template #head>
      <Icon class="head-icon" name="clock" />
      <span class="head-title">{{ t(noticeTitleKey, { count: interruptions.length }) }}</span>
      <IconButton
        class="close"
        size="sm"
        :label="t('recovery.closeNotice')"
        :title="t('recovery.closeNotice')"
        @click="emit('close')"
      />
    </template>

    <ul class="rows">
      <li v-for="entry in interruptions" :key="entry.sessionId" class="row">
        <button class="row-main" type="button" :title="t('recovery.openSession')" @click="emit('open-session', entry)">
          <span class="row-workspace">{{ entry.workspaceName }}</span>
          <span class="row-title">{{ entry.title }}</span>
          <span class="row-time">{{ relativeTime(entry) }}</span>
        </button>
        <Tooltip
          v-if="entry.pendingBackgroundHint"
          :text="t('recovery.checkBackgroundHint')"
          placement="top"
        >
          <Icon class="hint-icon" name="info" />
        </Tooltip>
        <Button size="sm" variant="secondary" @click="emit('resume', entry)">
          {{ t('recovery.resumeOne') }}
        </Button>
        <Button size="sm" variant="ghost" @click="emit('dismiss', entry)">
          {{ t('recovery.dismissOne') }}
        </Button>
      </li>
    </ul>

    <template #foot>
      <Button size="sm" variant="primary" @click="emit('resume-all')">
        {{ t('recovery.resumeAll', { count: interruptions.length }) }}
      </Button>
      <Button size="sm" variant="ghost" @click="emit('dismiss-all')">
        {{ t('recovery.dismissAll') }}
      </Button>
    </template>
  </Card>
</template>

<style scoped>
.recovery-notice {
  position: fixed;
  right: 16px;
  top: 16px;
  z-index: var(--z-toast);
  width: min(440px, calc(100vw - 32px));
  max-height: 56vh;
  display: flex;
  flex-direction: column;
}

.head-icon {
  flex: none;
  color: var(--color-text-muted);
}

.head-title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.close {
  margin-left: auto;
}

.rows {
  list-style: none;
  margin: 0;
  padding: 0;
  overflow-y: auto;
}

.row {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: 8px 0;
}

.row + .row {
  border-top: 1px solid var(--color-line);
}

.row-main {
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: baseline;
  gap: var(--space-2);
  padding: 2px 4px;
  border: 0;
  background: none;
  cursor: pointer;
  text-align: left;
  color: var(--color-text);
  font: inherit;
}

.row-main:hover .row-title {
  text-decoration: underline;
}

.row-workspace {
  flex: none;
  max-width: 34%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--color-text-muted);
  font-size: var(--ui-font-size-xs);
}

.row-title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: var(--ui-font-size-sm);
}

.row-time {
  flex: none;
  color: var(--color-text-muted);
  font-size: var(--ui-font-size-xs);
}

.hint-icon {
  flex: none;
  color: var(--color-text-muted);
}

@media (max-width: 640px) {
  .recovery-notice {
    left: 12px;
    right: 12px;
    top: 12px;
    width: auto;
  }
}
</style>
