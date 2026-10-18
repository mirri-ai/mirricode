<!-- apps/mirri-web/src/components/chat/NotificationNotice.vue -->
<!-- In-transcript notice for a background task's terminal notification (B2
     T11): the engine delivers the `<notification>` envelope as a user-role
     message with origin background_task, and this card is its only
     presentation. Structured after CronNotice (same bubble + meta row
     tokens), but left-aligned like a system notice — it is the engine
     reporting on the user's behalf, not a message the user scheduled. The
     bubble shows the i18n title and the settlement body; the meta row shows
     the terminal status word (completed → success green, everything else →
     warning amber, the TUI background-task-status color semantics), the
     task id, and the delivery time. -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import Icon from '../ui/Icon.vue';
import MessageTime from './MessageTime.vue';
import type { NotificationTurnData } from '../../types';

const props = defineProps<{
  text: string;
  notification: NotificationTurnData;
  /** ISO timestamp of when the notification was delivered (the turn's createdAt). */
  createdAt?: string;
}>();

const { t } = useI18n();

// Terminal status word → the i18n label; unknown future words surface raw
// (honest, never fabricated).
const statusWord = computed(() => {
  switch (props.notification.status) {
    case 'completed':
      return t('tasks.notice.statusCompleted');
    case 'failed':
      return t('tasks.notice.statusFailed');
    case 'killed':
      return t('tasks.notice.statusKilled');
    case 'timed_out':
      return t('tasks.notice.statusTimedOut');
    case 'lost':
      return t('tasks.notice.statusLost');
    default:
      return props.notification.status;
  }
});

// completed → success color (✓); failed / killed / timed_out / lost →
// warning color (⚠) — the TUI's background-task-status semantics.
const isCompleted = computed(() => props.notification.status === 'completed');
</script>

<template>
  <div class="nn notification-notice" :class="{ warn: !isCompleted }" role="status">
    <div class="nn-bubble">
      <span class="nn-title">{{ t('tasks.notice.title') }}</span>
      <template v-if="text"> <span class="nn-body">{{ text }}</span></template>
    </div>
    <div class="nn-meta">
      <span class="nn-status" :class="isCompleted ? 'ok' : 'warn'" :aria-label="statusWord">
        <Icon :name="isCompleted ? 'check' : 'alert-triangle'" size="sm" />
        <span class="nn-status-word">{{ statusWord }}</span>
      </span>
      <span
        class="nn-meta-item nn-id"
        :title="t('tasks.notice.task', { id: notification.taskId })"
      >{{ notification.taskId }}</span>
      <MessageTime v-if="createdAt" :time="createdAt" />
    </div>
  </div>
</template>

<style scoped>
.nn {
  margin: 0;
  align-self: flex-start;
  max-width: 78%;
  display: flex;
  flex-direction: column;
  align-items: flex-start;
}

/* The bubble is tinted by the terminal status: completed → the success
   scale, everything else → the warning scale (the TUI's
   background-task-status color semantics). */
.nn-bubble {
  box-sizing: border-box;
  max-width: 100%;
  padding: 8px 14px;
  background: var(--color-success-soft);
  border: 1px solid var(--color-success-bd);
  border-radius: var(--radius-xl) var(--radius-xl) var(--radius-xl) var(--radius-sm);
  box-shadow: var(--shadow-xs);
  color: var(--color-text);
  font-size: var(--content-font-size);
  line-height: var(--leading-normal);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.nn.warn .nn-bubble {
  background: var(--color-warning-soft);
  border-color: var(--color-warning-bd);
}
.nn-title {
  display: block;
  font-weight: var(--weight-medium);
  margin-bottom: 2px;
}

/* Meta row under the bubble, sized to match the user message's meta row. */
.nn-meta {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 4px;
  padding: 0 4px;
  color: var(--color-text-faint);
  font-size: var(--text-base);
  line-height: var(--leading-normal);
}
.nn-meta-item {
  white-space: nowrap;
}
.nn-status {
  display: inline-flex;
  align-items: center;
  gap: 3px;
}
.nn-status.ok {
  color: var(--color-success);
}
.nn-status.warn {
  color: var(--color-warning);
}
.nn-id {
  max-width: 220px;
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
