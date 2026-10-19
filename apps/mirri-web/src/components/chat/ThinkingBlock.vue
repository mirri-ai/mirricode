<!-- apps/mirri-web/src/components/chat/ThinkingBlock.vue -->
<!-- Two-phase presentation. While this block is streaming it shows a live
     scrolling window (max 2 lines). When the stream ends the block folds into
     a one-line "Thought · duration" header (Claude-style); clicking the header
     toggles an inline expand of the full text, clamped to a scrollable
     ~8-line window. -->
<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { iconSvg } from '../../lib/icons';
import { createScrollFollow } from '../../lib/scrollFollow';

const props = withDefaults(
  defineProps<{
    text: string;
    mobile?: boolean;
    streaming?: boolean;
    foldable?: boolean;
    /** Client-side measured thinking duration (ms) — shown in the folded
     * header after streaming ends. Absent for snapshot-loaded turns. */
    durationMs?: number;
  }>(),
  { mobile: false, streaming: false, foldable: true },
);

const { t } = useI18n();

// Live window while streaming, header + inline expand afterwards.
const paragraphs = computed(() =>
  props.text
    .split(/\n{2,}/)
    .filter((p) => p.trim().length > 0),
);

/** While streaming, lock to the simple <pre> path — no header, no expand UI.
 * The folded header only appears after the stream ends. */
const isFoldable = computed(
  () => props.foldable && !props.streaming && paragraphs.value.length > 1,
);

const expanded = ref(false);

const bodyEl = ref<HTMLElement | null>(null);

/** Humanized duration for the folded header ("3s", "1m 5s", …). */
const durationLabel = computed(() => {
  const ms = props.durationMs;
  if (ms == null || ms < 0) return null;
  const totalSeconds = Math.round(ms / 1000);
  if (totalSeconds < 1) return t('thinking.durationSeconds', { s: 1 });
  if (totalSeconds < 60) return t('thinking.durationSeconds', { s: totalSeconds });
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return seconds === 0
    ? t('thinking.durationMinutes', { m: minutes })
    : t('thinking.durationMinutesSeconds', { m: minutes, s: seconds });
});

const iconSparkles = iconSvg('sparkles', 'sm');
const iconChevronDown = iconSvg('chevron-down', 'sm');
const iconChevronUp = iconSvg('chevron-up', 'sm');

// Direction-based bottom follow (same convention as the main conversation
// pane): scrolling up stops the follow outright — with a distance-only check
// the next streaming delta would pin the live window back down before the
// user's upward scroll can accumulate.
const follow = createScrollFollow(() => bodyEl.value);

// On mount, a streaming block must land on its LATEST line. After a page refresh
// mid-stream the whole thinking text is present at once with scrollTop 0, so the
// follow state alone would otherwise leave the live window parked at the top.
// A static/historical block is left at its start (we don't pin it).
onMounted(() => {
  if (!props.streaming) return;
  follow.scrollToBottom();
});

watch(
  () => props.text,
  () => {
    if (!props.streaming) return;
    void follow.onContentGrow();
  },
  { immediate: true },
);

// Wheel/touch arrive BEFORE the browser dispatches `scroll` — stop the follow
// (and cancel any queued pin) so an in-flight streaming delta cannot yank the
// live window back down after the user flicked up. Only upward motion counts.
function onBodyWheel(event: WheelEvent): void {
  if (event.defaultPrevented || event.deltaY >= 0) return;
  follow.onUserIntent('up');
}

function onBodyTouch(): void {
  follow.onUserIntent('up');
}
</script>

<template>
  <div class="think" :class="{ mob: mobile, 'is-streaming': streaming }">
    <!-- Streaming: live scrolling window -->
    <pre
      v-if="streaming"
      ref="bodyEl"
      class="tc"
      @scroll.passive="follow.onScroll"
      @wheel.passive="onBodyWheel"
      @touchstart.passive="onBodyTouch"
    >{{ text }}</pre>
    <!-- Ended: header line, click toggles the inline expand -->
    <div v-else-if="isFoldable" class="fold">
      <button class="fold-header" type="button" @click="expanded = !expanded">
        <span class="fold-icon" v-html="iconSparkles" />
        <span class="fold-label">{{ t('thinking.label') }}</span>
        <span v-if="durationLabel" class="fold-duration">· {{ durationLabel }}</span>
        <span class="fold-chevron" v-html="expanded ? iconChevronUp : iconChevronDown" />
      </button>
      <pre
        v-if="expanded"
        ref="bodyEl"
        class="tc tc-expanded"
        @scroll.passive="follow.onScroll"
        @wheel.passive="onBodyWheel"
        @touchstart.passive="onBodyTouch"
      >{{ text }}</pre>
    </div>
    <!-- Single-paragraph or explicitly non-foldable: always show full content -->
    <pre
      v-else
      ref="bodyEl"
      class="tc tc-expanded"
      @scroll.passive="follow.onScroll"
      @wheel.passive="onBodyWheel"
      @touchstart.passive="onBodyTouch"
    >{{ text }}</pre>
  </div>
</template>

<style scoped>
.think {
  margin: 0;
}

/* ---- Streaming live window ---- */
.tc {
  font: var(--text-base)/var(--leading-relaxed) var(--font-mono);
  color: var(--color-text-muted);
  white-space: pre-wrap;
  word-break: break-word;
  margin: 0;
  max-height: calc(var(--leading-relaxed) * 1em * 2);
  overflow-y: scroll;
  scrollbar-gutter: stable;
  overflow-anchor: none;
}

/* ---- Ended: folded header + inline expand ---- */
.fold {
  display: flex;
  flex-direction: column;
}

.fold-header {
  display: inline-flex;
  align-items: center;
  gap: 0.4em;
  width: fit-content;
  padding: 0;
  border: none;
  background: none;
  font: var(--text-sm)/var(--leading-normal) var(--font-ui);
  color: var(--color-text-faint);
  cursor: pointer;
  user-select: none;
}

.fold-header:hover {
  color: var(--color-text);
}

.fold-icon {
  display: inline-flex;
}

.fold-label {
  color: var(--color-text-faint);
}

.fold-duration {
  color: var(--color-text-faint);
}

.fold-chevron {
  display: inline-flex;
  color: var(--color-text-faint);
}

/* Expanded body: scrollable, clamped to ~8 lines */
.tc-expanded {
  margin-top: 0.25em;
  max-height: calc(var(--leading-relaxed) * 1em * 8);
}

/* ---- Mobile tweaks ---- */
.mob {
  margin: 0;
}
.mob .tc {
  color: var(--color-text-faint);
  line-height: var(--leading-normal);
  max-height: calc(var(--leading-normal) * 1em * 2);
}
.mob .tc-expanded {
  max-height: calc(var(--leading-normal) * 1em * 8);
}
</style>
