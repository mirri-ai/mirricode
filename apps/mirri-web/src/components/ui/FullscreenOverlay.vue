<!-- apps/mirri-web/src/components/ui/FullscreenOverlay.vue -->
<!-- Design-system §03b FullscreenOverlay: a full-viewport takeover layer used
     for "places" like Settings that need the entire window (including the
     sidebar) rather than a centered modal. Renders above the workspace grid
     via Teleport to body — the workspace DOM stays mounted underneath, so
     scroll position, drafts, and streaming state are all preserved.

     Unlike Dialog, this is not a centered panel with overlay scrim — it fills
     `inset: 0` and owns its own layout. Provides a topbar slot (with optional
     macOS drag region for traffic-light clearance) and a body slot. -->
<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue';
import { openDialogCount } from '../../composables/dialogStack';
import { isMacosDesktop } from '../../lib/desktopFlag';

withDefaults(
  defineProps<{
    /** When true, the topbar gets `-webkit-app-region: drag` on macOS desktop
     *  so the user can drag the window by it. Interactive elements inside the
     *  topbar should use `.no-drag` to opt out. */
    topbarDrag?: boolean;
    /** ARIA role. Defaults to 'dialog' with aria-modal=true. */
    role?: string;
  }>(),
  {
    topbarDrag: true,
    role: 'dialog',
  },
);

const emit = defineEmits<{
  /** Emitted on Escape keydown. The parent decides whether to close. */
  escape: [];
}>();

const root = ref<HTMLElement | null>(null);
let previouslyFocused: HTMLElement | null = null;

function onKeydown(e: KeyboardEvent): void {
  if (e.key !== 'Escape') return;
  // 当嵌套的 Dialog/modal 叠在本层之上时，让上层拥有 Escape。
  // openDialogCount 把本层算作 1，任何叠在上面的 Dialog 会让计数 > 1；
  // 上层 Dialog 自己有 closeOnEsc 处理，这里不抢，避免一次 ESC 把 Settings
  // 连同其内嵌 Dialog 一起关掉。
  if (openDialogCount.value > 1) return;
  emit('escape');
}

onMounted(() => {
  // Participate in the global dialog counter so App.vue's anyOverlayOpen
  // correctly suppresses background Escape handlers while we're open.
  openDialogCount.value += 1;
  previouslyFocused =
    typeof document !== 'undefined' && document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
  document.addEventListener('keydown', onKeydown);
  // Move focus into the overlay so keyboard navigation works.
  const target = root.value;
  if (target) {
    requestAnimationFrame(() => {
      const focusable = target.querySelector<HTMLElement>(
        '[data-initial-focus], button, [tabindex]:not([tabindex="-1"])',
      );
      focusable?.focus();
    });
  }
});

onBeforeUnmount(() => {
  openDialogCount.value = Math.max(0, openDialogCount.value - 1);
  document.removeEventListener('keydown', onKeydown);
  if (previouslyFocused && typeof document !== 'undefined' && document.contains(previouslyFocused)) {
    previouslyFocused.focus();
    previouslyFocused = null;
  }
});
</script>

<template>
  <Teleport to="body">
    <section
      ref="root"
      class="fs-overlay"
      :role="role"
      aria-modal="true"
      tabindex="-1"
    >
      <header
        v-if="$slots.topbar"
        class="fs-overlay__topbar"
        :class="{ 'macos-desktop': topbarDrag && isMacosDesktop }"
      >
        <slot name="topbar" />
      </header>
      <div class="fs-overlay__body">
        <slot />
      </div>
    </section>
  </Teleport>
</template>

<style scoped>
.fs-overlay {
  position: fixed;
  inset: 0;
  z-index: var(--z-modal);
  display: flex;
  flex-direction: column;
  background: var(--color-surface);
  outline: none;
  animation: fs-overlay-in var(--duration-base) var(--ease-out);
}
@keyframes fs-overlay-in {
  from { opacity: 0; transform: scale(0.98); }
  to { opacity: 1; transform: scale(1); }
}
.fs-overlay__topbar {
  height: 48px;
  flex: none;
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: 0 var(--space-4);
  padding-left: max(var(--space-4), var(--safe-left, 0px));
  border-bottom: 1px solid var(--color-line);
}
/* macOS desktop: allow window dragging from the topbar. Interactive elements
   inside must opt out with .no-drag. The traffic lights (~70px × ~38px) float
   over the top-left; on macOS we grow the topbar and push content below/right
   so the back button sits clear of the lights. */
.macos-desktop {
  -webkit-app-region: drag;
  height: 52px;
  padding-top: max(28px, env(safe-area-inset-top, 0px));
  padding-left: max(80px, var(--safe-left, 0px));
  align-items: flex-end;
  padding-bottom: 4px;
}
.macos-desktop :deep(.no-drag) {
  -webkit-app-region: no-drag;
}
.fs-overlay__body {
  flex: 1;
  min-height: 0;
  display: flex;
  overflow: hidden;
}
</style>
