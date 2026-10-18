<!-- apps/mirri-web/src/components/chat/ThinkingPanel.vue -->
<!-- Full thinking text in the right-side panel (App's shared preview slot —
     opening this replaces a file preview and vice versa). Content is reactive:
     while the block is still streaming the text keeps growing, and the body
     follows the bottom as long as the user hasn't scrolled up. -->
<script setup lang="ts">
import { ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import PanelHeader from '../ui/PanelHeader.vue';
import { createScrollFollow } from '../../lib/scrollFollow';

const props = defineProps<{
  text: string;
  /** Header label override — defaults to the thinking panel title. Lets the
      panel double as the compaction-summary viewer. */
  subtitle?: string;
}>();

const emit = defineEmits<{
  close: [];
}>();

const { t } = useI18n();

const bodyEl = ref<HTMLElement | null>(null);
// Direction-based bottom follow (same convention as the main conversation
// pane): scrolling up stops the follow outright, so streaming text can never
// pin the view back down over the user's upward intent.
const follow = createScrollFollow(() => bodyEl.value);
watch(
  () => props.text,
  () => void follow.onContentGrow(),
  { immediate: true },
);

// Wheel/touch arrive BEFORE the browser dispatches `scroll` — stop the follow
// (and cancel any queued pin) so an in-flight streaming delta cannot yank the
// view back down after the user flicked up. Only upward motion counts.
function onBodyWheel(event: WheelEvent): void {
  if (event.defaultPrevented || event.deltaY >= 0) return;
  follow.onUserIntent('up');
}

function onBodyTouch(): void {
  follow.onUserIntent('up');
}
</script>

<template>
  <div class="tp">
    <PanelHeader
      :title="t('common.preview')"
      :subtitle="subtitle ?? t('thinking.panelTitle')"
      :close-label="t('thinking.close')"
      @close="emit('close')"
    />
    <pre
      ref="bodyEl"
      class="tp-body"
      @scroll.passive="follow.onScroll"
      @wheel.passive="onBodyWheel"
      @touchstart.passive="onBodyTouch"
    >{{ text }}</pre>
  </div>
</template>

<style scoped>
.tp {
  height: 100%;
  display: flex;
  flex-direction: column;
  min-height: 0;
  background: var(--color-bg);
}

.tp-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  margin: 0;
  padding: 12px 14px;
  font: var(--text-base)/var(--leading-relaxed) var(--font-mono);
  color: var(--color-text-muted);
  white-space: pre-wrap;
  word-break: break-word;
}
</style>
