<!-- apps/mirri-web/src/components/chat/OpenInButton.vue -->
<!-- Header "Open In" split button (open-in-apps Task 4): quick-open (last
     used target) + dropdown listing detected apps with their extracted
     logos. Hidden entirely in a plain browser (no desktop bridge) or when
     nothing was detected. -->
<script lang="ts">
// Plain named-export block: the repo has no @vue/test-utils, so the button's
// decision logic is a pure named export and tested directly with vitest
// (test/open-in-button.test.ts), following the TurnUsagePill precedent.
export interface DetectedApp {
  appId: string;
  name: string;
  iconDataUrl?: string;
}

export interface OpenInViewModel {
  /** Whether the button renders at all (at least one detected app). */
  visible: boolean;
  /** The last-used target app, or null when no/unknown last target. */
  target: DetectedApp | null;
  /** The last-used app's extracted logo, or null for the icon fallback. */
  buttonIcon: string | null;
  /** Which fallback icon the collapsed button shows when there is no logo. */
  buttonIconKind: 'folder' | 'external-link';
  /** The app id a quick-open click resolves to, or null (nothing to open). */
  quickOpenAppId: string | null;
  /** No work directory disables the button. */
  disabled: boolean;
}

/** Resolve the button's collapsed state from the detected apps, the stored
 *  last target, and whether a work directory exists. A last target that no
 *  longer matches any detected app falls back to the first app for
 *  quick-open but renders the generic external-link icon. */
export function buildOpenInViewModel(
  apps: DetectedApp[],
  lastTarget: string | null,
  hasWorkDir: boolean,
): OpenInViewModel {
  const target = lastTarget === null ? null : apps.find((a) => a.appId === lastTarget) ?? null;
  return {
    visible: apps.length > 0,
    target,
    buttonIcon: target?.iconDataUrl ?? null,
    buttonIconKind: target === null ? 'external-link' : 'folder',
    quickOpenAppId: target?.appId ?? apps[0]?.appId ?? null,
    disabled: !hasWorkDir,
  };
}
</script>

<script setup lang="ts">
import { computed, nextTick, onUnmounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';

import Icon from '../ui/Icon.vue';

const { t } = useI18n();

const props = defineProps<{
  apps: DetectedApp[];
  lastTarget: string | null;
  hasWorkDir: boolean;
}>();

const emit = defineEmits<{ openInApp: [appId: string] }>();

const vm = computed(() => buildOpenInViewModel(props.apps, props.lastTarget, props.hasWorkDir));
// last-target lookup lives in the view-model (single source, no duplicate find).
const lastTargetApp = computed(() => vm.value.target);

// --- dropdown（fixed-positioning + viewport-clamping 定位手法） ---
const menuOpen = ref(false);
const menuStyle = ref<Record<string, string>>({});
const btnRef = ref<HTMLElement | null>(null);
const menuRef = ref<HTMLElement | null>(null);

function onDocClick(e: MouseEvent): void {
  const target = e.target as Node;
  if (menuRef.value?.contains(target) || btnRef.value?.contains(target)) return;
  closeMenu();
}

function onDocKeydown(e: KeyboardEvent): void {
  if (e.key === 'Escape') closeMenu();
}

function closeMenu(): void {
  menuOpen.value = false;
  document.removeEventListener('mousedown', onDocClick);
  document.removeEventListener('keydown', onDocKeydown);
  window.removeEventListener('resize', closeMenu);
}

async function toggleMenu(): Promise<void> {
  if (menuOpen.value) {
    closeMenu();
    return;
  }
  menuOpen.value = true;
  document.addEventListener('mousedown', onDocClick);
  document.addEventListener('keydown', onDocKeydown);
  window.addEventListener('resize', closeMenu);
  await nextTick();
  const btn = btnRef.value;
  const menu = menuRef.value;
  if (!btn || !menu) return;
  const r = btn.getBoundingClientRect();
  const margin = 8;
  const menuW = menu.offsetWidth;
  const menuH = menu.offsetHeight;
  let top = r.bottom + 4;
  if (top + menuH > window.innerHeight - margin) {
    top = Math.max(margin, r.top - menuH - 4);
  }
  let left = r.right - menuW;
  if (left < margin) left = margin;
  menuStyle.value = { top: `${Math.round(top)}px`, left: `${Math.round(left)}px` };
}

onUnmounted(() => {
  document.removeEventListener('mousedown', onDocClick);
  document.removeEventListener('keydown', onDocKeydown);
  window.removeEventListener('resize', closeMenu);
});

function quickOpen(): void {
  if (!props.hasWorkDir) return;
  if (menuOpen.value) {
    closeMenu(); // 主体点击在菜单开着时是收起，不触发打开
    return;
  }
  const appId = vm.value.quickOpenAppId;
  if (appId !== null) emit('openInApp', appId);
}

function pick(app: DetectedApp): void {
  closeMenu();
  if (props.hasWorkDir) emit('openInApp', app.appId);
}
</script>

<template>
  <div v-if="vm.visible" ref="btnRef" class="open-in" :class="{ open: menuOpen }">
    <button
      type="button"
      class="open-in-btn"
      :disabled="vm.disabled"
      :aria-label="t('header.openInApps')"
      @click.stop="quickOpen"
    >
      <img
        v-if="vm.buttonIcon !== null"
        class="open-in-icon"
        :src="vm.buttonIcon"
        :alt="lastTargetApp?.name"
      />
      <Icon v-else-if="vm.buttonIconKind === 'folder'" name="folder" size="sm" />
      <Icon v-else name="external-link" size="sm" />
    </button>
    <button
      type="button"
      class="open-in-caret-btn"
      :disabled="vm.disabled"
      aria-haspopup="menu"
      :aria-expanded="menuOpen"
      :aria-label="t('header.chooseOpenApp')"
      @click.stop="toggleMenu"
    >
      <Icon name="chevron-down" size="sm" class="open-in-caret" />
    </button>
    <div
      v-if="menuOpen"
      ref="menuRef"
      class="open-in-menu"
      :style="menuStyle"
      role="menu"
      @click.stop
    >
      <button
        v-for="app in apps"
        :key="app.appId"
        type="button"
        class="open-in-item"
        :class="{ active: app.appId === lastTargetApp?.appId }"
        role="menuitem"
        @click="pick(app)"
      >
        <img v-if="app.iconDataUrl" class="open-in-item-icon" :src="app.iconDataUrl" :alt="app.name" />
        <Icon v-else name="external-link" size="sm" />
        <span class="open-in-item-name">{{ app.name }}</span>
        <Icon v-if="app.appId === lastTargetApp?.appId" name="check" size="sm" class="open-in-check" />
      </button>
    </div>
  </div>
</template>

<style scoped>
.open-in { position: relative; flex: none; display: inline-flex; height:68% }
/* The chat header doubles as the macOS window-drag region; ChatHeader's
   no-drag exemption is scoped to its own template and does not reach this
   child component's buttons (or the fixed-position dropdown), so opt out
   here or every click becomes a window drag. */
.open-in,
.open-in-menu {
  -webkit-app-region: no-drag;
}
.open-in-btn {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  height: 100%;
  padding: 0 6px;
  border: 1px solid var(--color-line);
  border-radius: 6px 0 0 6px;
  background: var(--color-bg);
  color: var(--dim);
  cursor: pointer;
}
.open-in-btn:hover:not(:disabled) { color: var(--color-text); border-color: var(--dim); }
.open-in-btn:disabled { opacity: 0.5; cursor: default; }
.open-in-icon { width: auto; height: 100%; border-radius: 3px; }

/* Dedicated left-click caret trigger (Chromium dispatches contextmenu, not
   click, on right-click — the menu must open on a plain left click). */
.open-in-caret-btn {
  display: inline-flex;
  align-items: center;
  height: 100%;
  padding: 0 3px;
  border: 1px solid var(--color-line);
  border-left: none;
  border-radius: 0 6px 6px 0;
  background: var(--color-bg);
  color: var(--dim);
  cursor: pointer;
}
.open-in-caret-btn:hover:not(:disabled) { color: var(--color-text); }
.open-in-caret-btn:disabled { opacity: 0.5; cursor: default; }
.open-in-caret { opacity: 0.6; }
.open-in.open :is(.open-in-btn, .open-in-caret-btn) { color: var(--color-text); }

.open-in-menu {
  position: fixed;
  top: 0;
  left: 0;
  z-index: var(--z-dropdown);
  min-width: 180px;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 4px;
  border: 1px solid var(--color-line);
  border-radius: 8px;
  background: var(--color-bg);
  box-shadow: 0 8px 24px rgb(0 0 0 / 0.25);
}
.open-in-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 8px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: var(--color-text);
  font-size: var(--ui-font-size);
  text-align: left;
  cursor: pointer;
}
.open-in-item:hover { background: var(--color-surface-raised); }
.open-in-item-icon { width: 18px; height: 18px; border-radius: 4px; }
.open-in-item-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.open-in-check { color: var(--color-accent); }
</style>
