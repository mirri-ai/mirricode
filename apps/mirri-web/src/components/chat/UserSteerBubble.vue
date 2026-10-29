<!-- apps/mirri-web/src/components/chat/UserSteerBubble.vue -->
<!-- A user message steered into the RUNNING turn (ADR-010 / CR13): a full
     right-aligned user bubble rendered inline inside the assistant turn's
     block chain — attachment chips, verbatim text, a "steer" badge and its
     own timestamp.
     Styles are SELF-CARRIED (the CronNotice pattern): ChatPane's .u-turn /
     .u-bub / .u-text / .u-atts / .u-meta rules live in its <style scoped>,
     and scoped CSS only reaches a child component's root node — relying on
     them left this bubble with no fill/border/radius/padding. The rules below
     are copied from ChatPane and adapted (see its lines ~1159-1197 / ~1469). -->
<script setup lang="ts">
import { useI18n } from 'vue-i18n';
import type { TurnAttachment } from '../../types';
import AttachmentChip from './AttachmentChip.vue';
import MessageTime from './MessageTime.vue';

defineProps<{
  text: string;
  attachments: TurnAttachment[];
  createdAt: string;
}>();

const emit = defineEmits<{
  activateAttachment: [att: TurnAttachment];
}>();

const { t } = useI18n();
</script>

<template>
  <div class="steer-turn">
    <div class="steer-bub">
      <div v-if="attachments.length > 0" class="steer-atts">
        <AttachmentChip
          v-for="(att, ai) in attachments"
          :key="ai"
          :kind="att.kind"
          :name="att.name"
          :url="att.url"
          :file-id="att.fileId"
          :media-type="att.mediaType"
          :size="att.size"
          @activate="emit('activateAttachment', att)"
        />
      </div>
      <div class="steer-text">{{ text }}</div>
    </div>
    <div class="steer-meta">
      <span class="steer-badge">{{ t('conversation.steerBadge') }}</span>
      <MessageTime v-if="createdAt" :time="createdAt" />
    </div>
  </div>
</template>

<style scoped>
/* Mirrors .u-turn: a right-aligned column wrapping bubble + meta row. The
   assistant column (.a-msg) is width:94%, so the steer bubble right-aligns
   within it. */
.steer-turn {
  display: flex;
  flex-direction: column;
  align-items: flex-end;
  align-self: flex-end;
  width: 100%;
}

/* Mirrors .u-bub: right-aligned soft-blue bubble (accent fill + border,
   rounded with one small corner, soft shadow, capped width). */
.steer-bub {
  box-sizing: border-box;
  align-self: flex-end;
  max-width: 78%;
  background: var(--color-accent-soft);
  border: 1px solid var(--color-accent-bd);
  color: var(--color-text);
  border-radius: var(--radius-xl) var(--radius-xl) var(--radius-sm) var(--radius-xl);
  padding: 11px 15px;
  font-size: var(--content-font-size);
  line-height: var(--leading-normal);
  box-shadow: var(--shadow-xs);
}
@media (max-width: 640px) {
  .steer-bub {
    max-width: min(88%, calc(100vw - 52px));
  }
}

/* Mirrors .u-atts: chip row above the bubble text. */
.steer-atts {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 8px;
}

/* Mirrors .u-text: verbatim user input — preserve newlines, break long tokens. */
.steer-text {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

/* Mirrors .u-meta: right-aligned meta row under the bubble. */
.steer-meta {
  align-self: flex-end;
  display: flex;
  justify-content: flex-end;
  align-items: center;
  gap: 6px;
  max-width: 78%;
  margin-top: 2px;
  margin-right: 4px;
}

/* The steer badge — the one visual element a normal user turn lacks: a
   compact accent pill marking this as a mid-turn steer, not a new prompt. */
.steer-badge {
  flex: none;
  display: inline-flex;
  align-items: center;
  padding: 1px 8px;
  border-radius: var(--radius-sm);
  background: var(--color-accent-soft);
  border: 1px solid var(--color-accent-bd);
  color: var(--color-accent);
  font-size: var(--ui-font-size-sm);
  line-height: 1.4;
}
</style>
