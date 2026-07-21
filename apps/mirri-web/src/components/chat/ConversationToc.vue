<!-- apps/mirri-web/src/components/chat/ConversationToc.vue -->
<!-- Floating outline: collapsed state shows a compact bar cluster pinned to the
     right-middle edge. On hover/focus the panel expands into a floating card
     with scrollable, labeled rows. No longer anchored to the reading-column
     edge — it floats at the right side so it works at any width. -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import type { ChatTurn } from '../../types';

export interface ConversationTocItem {
  id: string;
  role: ChatTurn['role'];
  no: number;
  title: string;
}

const props = defineProps<{
  items: ConversationTocItem[];
  /** Query currently owning the viewport middle. */
  activeTurnId: string | null;
  mobile?: boolean;
  sessionLoading?: boolean;
  /** Temporarily hidden while a wide table actually covers the rail. Kept out
      of `visible` on purpose: the nav must stay mounted so the occlusion can
      be measured and lifted again. Never touches the user's TOC setting. */
  occluded?: boolean;
}>();

const emit = defineEmits<{
  select: [turnId: string];
}>();

const { t } = useI18n();

// The outline is only useful once there is something to navigate, and it never
// shows on mobile or while the session is still loading.
const visible = computed(
  () => !props.mobile && !props.sessionLoading && props.items.length > 1,
);
</script>

<template>
  <!-- Conversation outline: collapsed dot cluster at the right-middle edge;
       hover to expand into a floating scrollable panel of labeled rows. -->
  <nav
    v-if="visible"
    class="conversation-toc"
    :class="{ 'toc-occluded': occluded }"
    :aria-label="t('conversation.toc')"
  >
    <div class="toc-scroll">
      <button
        v-for="item in items"
        :key="item.id"
        type="button"
        class="toc-row"
        :class="{ active: activeTurnId === item.id }"
        @click="emit('select', item.id); ($event.currentTarget as HTMLButtonElement).blur()"
      >
        <span class="toc-bar" />
        <span class="toc-label">{{ item.title }}</span>
      </button>
    </div>
  </nav>
</template>

<style scoped>
.conversation-toc {
  position: absolute;
  z-index: var(--z-sticky);
  top: 50%;
  right: 12px;
  transform: translateY(-50%);
  display: flex;
  flex-direction: column;
  /* Collapsed: narrow rail of dot bars. */
  width: auto;
  max-height: calc(100vh - 160px);
  opacity: 0.45;
  transition: opacity var(--duration-base) var(--ease-out);
}
.conversation-toc:hover,
.conversation-toc:focus-within { opacity: 1; }

/* Invisible hover bridge to make the collapsed rail easy to enter. */
.conversation-toc::before {
  content: "";
  position: absolute;
  top: -8px;
  bottom: -8px;
  left: -12px;
  right: -12px;
  z-index: 0;
}

.toc-scroll {
  position: relative;
  z-index: 1;
  display: flex;
  flex-direction: column;
  gap: 7px;
  padding: 6px;
  border-radius: var(--radius-md);
  transition:
    background var(--duration-base) var(--ease-out),
    box-shadow var(--duration-base) var(--ease-out);
}

/* Collapsed: only the dots show, no background. */
.toc-row {
  display: flex;
  align-items: center;
  gap: 10px;
  height: 18px;
  padding: 0;
  border: none;
  background: transparent;
  color: var(--color-text-muted);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  text-align: left;
  cursor: pointer;
  white-space: nowrap;
}
.toc-row:focus-visible { outline: none; box-shadow: var(--p-focus-ring); }

.toc-bar {
  flex: none;
  width: 4px;
  height: 14px;
  border-radius: var(--radius-full);
  background: var(--color-accent);
  opacity: 0.35;
  transition:
    opacity var(--duration-fast) var(--ease-out),
    height var(--duration-fast) var(--ease-out),
    width var(--duration-fast) var(--ease-out);
}

.toc-label {
  display: block;
  max-width: 0;
  overflow: hidden;
  opacity: 0;
  text-overflow: ellipsis;
  transition:
    max-width 220ms var(--ease-out),
    opacity var(--duration-fast) var(--ease-out),
    color var(--duration-fast) var(--ease-out);
}

/* Active row always shows an accent bar. */
.toc-row.active .toc-bar { opacity: 1; height: 18px; }
.toc-row.active .toc-label { color: var(--color-accent); font-weight: var(--weight-medium); }

/* ---- Expanded (hover/focus) ---- */
.conversation-toc:hover .toc-scroll,
.conversation-toc:focus-within .toc-scroll {
  background: var(--color-surface-raised);
  box-shadow: var(--shadow-md);
}

.conversation-toc:hover .toc-bar,
.conversation-toc:focus-within .toc-bar {
  height: 18px;
  opacity: 0.5;
  width: 3px;
}
.conversation-toc:hover .toc-label,
.conversation-toc:focus-within .toc-label {
  max-width: 220px;
  opacity: 1;
}

.toc-row:hover .toc-bar { opacity: 1; }
.toc-row:hover .toc-label { color: var(--color-text); }

/* Occluded by a wide table — hide from view and interaction but keep mounted. */
.conversation-toc.toc-occluded {
  visibility: hidden;
  pointer-events: none;
}
</style>
