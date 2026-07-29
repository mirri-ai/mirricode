<!-- apps/mirri-web/src/components/chat/ChatHeader.vue -->
<!-- Right-column title bar: workspace/session breadcrumb, git status, PR badge,
     and a ⋮ more-menu with session actions. When the sidebar is collapsed,
     dynamic show-sidebar + new-chat buttons appear on the left. On macOS the
     header doubles as a window-drag region with traffic-light padding. -->
<script setup lang="ts">
import { computed, nextTick, onUnmounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { copyTextToClipboard } from '../../lib/clipboard';
import { isMacosDesktop, isWindowsDesktop } from '../../lib/desktopFlag';
import Menu from '../ui/Menu.vue';
import MenuItem from '../ui/MenuItem.vue';
import IconButton from '../ui/IconButton.vue';
import Icon from '../ui/Icon.vue';
import Tooltip from '../ui/Tooltip.vue';
import RenameDialog from '../dialogs/RenameDialog.vue';
import { useConfirmDialog } from '../../composables/useConfirmDialog';

const { t } = useI18n();
const { confirm } = useConfirmDialog();

const props = defineProps<{
  sessionId?: string;
  workspaceName?: string;
  /** Absolute path to the active workspace root. */
  workspaceRoot?: string;
  sessionTitle?: string;
  branch?: string;
  ahead?: number;
  behind?: number;
  changesCount?: number;
  /** Git diff line stats: additions / deletions. Zero/null values are hidden. */
  gitDiffStats?: { totalAdditions: number; totalDeletions: number } | null;
  isGitRepo?: boolean;
  /** GitHub PR for the current branch, when known (null/undefined = none). */
  pr?: { number: number; state: string; url: string } | null;
  /** True for ~2s after a successful copy-all, to flip the icon to a check. */
  copied?: boolean;
  /** True when the sidebar column is collapsed — shows show-sidebar +
   *  new-chat buttons and macOS traffic-light padding. */
  sidebarCollapsed?: boolean;
  /** Absolute path to the session's physical storage directory. */
  sessionDir?: string;
}>();

const emit = defineEmits<{
  copyAll: [];
  copyFinalSummary: [];
  openChanges: [];
  openPr: [url: string];
  renameSession: [id: string, title: string];
  forkSession: [id: string];
  archiveSession: [id: string];
  /** Show the sidebar column (when collapsed). */
  showSidebar: [];
  /** Start a new conversation. */
  newChat: [];
  /** Open the session workspace in an external app (finder, vscode, …). */
  openInApp: [appId: string];
}>();

const ahead = computed(() => props.ahead ?? 0);
const behind = computed(() => props.behind ?? 0);
const adds = computed(() => props.gitDiffStats?.totalAdditions ?? 0);
const dels = computed(() => props.gitDiffStats?.totalDeletions ?? 0);
const hasLineStats = computed(() => adds.value > 0 || dels.value > 0);

// ---------------------------------------------------------------------------
// More-menu (kebab dropdown)
// ---------------------------------------------------------------------------
const menuOpen = ref(false);
const kebabRef = ref<InstanceType<typeof IconButton> | null>(null);
const menuRef = ref<InstanceType<typeof Menu> | null>(null);
const menuStyle = ref<Record<string, string>>({});

function onDocClick(e: MouseEvent): void {
  const target = e.target as Node;
  if (menuRef.value?.el?.contains(target) || kebabRef.value?.el?.contains(target)) return;
  closeMenu();
}

function onScrollOrResize(): void {
  closeMenu();
}

async function toggleMenu(e: Event): Promise<void> {
  e.stopPropagation();
  if (menuOpen.value) {
    closeMenu();
    return;
  }
  menuOpen.value = true;
  document.addEventListener('mousedown', onDocClick);
  window.addEventListener('resize', onScrollOrResize);
  await nextTick();
  const btn = kebabRef.value?.el;
  const menu = menuRef.value?.el;
  if (!btn || !menu) return;
  const r = btn.getBoundingClientRect();
  const gap = 4;
  const margin = 8;
  const menuW = menu.offsetWidth;
  const menuH = menu.offsetHeight;
  let top = r.bottom + gap;
  if (top + menuH > window.innerHeight - margin) {
    top = Math.max(margin, r.top - menuH - gap);
  }
  let left = r.left;
  if (left + menuW > window.innerWidth - margin) {
    left = Math.max(margin, r.right - menuW);
  }
  menuStyle.value = {
    top: `${Math.round(top)}px`,
    left: `${Math.round(left)}px`,
  };
}

function closeMenu(): void {
  menuOpen.value = false;
  document.removeEventListener('mousedown', onDocClick);
  window.removeEventListener('resize', onScrollOrResize);
}

onUnmounted(() => {
  document.removeEventListener('mousedown', onDocClick);
  window.removeEventListener('resize', onScrollOrResize);
});

function onCopyAll(): void {
  emit('copyAll');
  closeMenu();
}

function onCopyFinalSummary(): void {
  emit('copyFinalSummary');
  closeMenu();
}

// ---------------------------------------------------------------------------
// Fork
// ---------------------------------------------------------------------------
function forkSession(): void {
  if (!props.sessionId) return;
  closeMenu();
  emit('forkSession', props.sessionId);
}

// ---------------------------------------------------------------------------
// Copy session ID
// ---------------------------------------------------------------------------
const copiedId = ref(false);
function copySessionId(): void {
  if (!props.sessionId) return;
  void copyTextToClipboard(props.sessionId).then((ok) => {
    if (!ok) return;
    copiedId.value = true;
    setTimeout(() => {
      copiedId.value = false;
    }, 1200);
  });
}

// ---------------------------------------------------------------------------
// Copy workspace path
// ---------------------------------------------------------------------------
const copiedPath = ref(false);
function copyWorkspacePath(): void {
  if (!props.workspaceRoot) return;
  void copyTextToClipboard(props.workspaceRoot).then((ok) => {
    if (!ok) return;
    copiedPath.value = true;
    setTimeout(() => {
      copiedPath.value = false;
    }, 1200);
  });
  closeMenu();
}

// ---------------------------------------------------------------------------
// Copy session storage path
// ---------------------------------------------------------------------------
const copiedSessionPath = ref(false);
function copySessionPath(): void {
  if (!props.sessionDir) return;
  void copyTextToClipboard(props.sessionDir).then((ok) => {
    if (!ok) return;
    copiedSessionPath.value = true;
    setTimeout(() => {
      copiedSessionPath.value = false;
    }, 1200);
  });
  closeMenu();
}

// ---------------------------------------------------------------------------
// Open in Finder / Explorer
// ---------------------------------------------------------------------------
function openInFileManager(): void {
  closeMenu();
  emit('openInApp', 'finder');
}

const openInFileManagerLabel = computed(() =>
  isWindowsDesktop ? t('header.openInExplorer') : t('header.openInFinder'),
);

// ---------------------------------------------------------------------------
// Rename — modal dialog
// ---------------------------------------------------------------------------
const renameDialogOpen = ref(false);

function startRename(): void {
  closeMenu();
  if (!props.sessionId) return;
  renameDialogOpen.value = true;
}

function onRenameConfirm(newTitle: string): void {
  if (props.sessionId && newTitle !== (props.sessionTitle ?? '').trim()) {
    emit('renameSession', props.sessionId, newTitle);
  }
  renameDialogOpen.value = false;
}

// ---------------------------------------------------------------------------
// Archive — modal confirm (the header has no session row to swap, so use the
// shared ConfirmDialog instead of the inline strip used in SessionRow).
// ---------------------------------------------------------------------------
async function startArchive(): Promise<void> {
  if (!props.sessionId) return;
  closeMenu();
  if (
    await confirm({
      title: t('header.archiveTask'),
      message: t('sidebar.archiveConfirm'),
      variant: 'danger',
    })
  ) {
    emit('archiveSession', props.sessionId);
  }
}
</script>

<template>
  <header class="chat-header" :class="{ 'macos-desktop': isMacosDesktop, 'sidebar-hidden': sidebarCollapsed }">
    <!-- Dynamic left section: show-sidebar + new-chat (only when sidebar collapsed) -->
    <template v-if="sidebarCollapsed">
      <IconButton
        class="ch-sidebar-toggle"
        :label="t('sidebar.expandSidebar')"
        @click="emit('showSidebar')"
      >
        <Icon name="panel-expand" size="sm" />
      </IconButton>
      <IconButton
        class="ch-new-chat"
        :label="t('sidebar.newChat')"
        @click="emit('newChat')"
      >
        <Icon name="chat-new" size="sm" />
      </IconButton>
    </template>

    <!-- Workspace / session breadcrumb (only when a session is active) -->
    <div v-if="sessionId" class="ch-id">
      <span v-if="workspaceName" class="ch-ws">{{ workspaceName }}</span>
      <span v-if="workspaceName && sessionTitle" class="ch-sep">/</span>
      <Tooltip v-if="sessionTitle" :text="sessionTitle">
        <span class="ch-ses">{{ sessionTitle }}</span>
      </Tooltip>
    </div>

    <!-- More menu trigger: session actions (only when a session is active) -->
    <IconButton
      v-if="sessionId"
      ref="kebabRef"
      class="ch-act-more"
      :class="{ open: menuOpen }"
      :label="t('header.options')"
      :aria-expanded="menuOpen"
      aria-haspopup="menu"
      @click.stop="toggleMenu($event)"
    >
      <Icon name="dots-horizontal" size="md" />
    </IconButton>

    <!-- Fixed more menu -->
    <Menu
      v-if="menuOpen && sessionId"
      ref="menuRef"
      class="ch-menu"
      :style="menuStyle"
      @click.stop
    >
      <!-- Conversation actions -->
      <MenuItem @click="onCopyAll">
        {{ copied ? t('header.copied') : t('header.copyAll') }}
      </MenuItem>
      <MenuItem @click="onCopyFinalSummary">
        {{ t('header.copyFinalSummary') }}
      </MenuItem>
      <MenuItem separator />
      <!-- Session management -->
      <MenuItem @click="startRename">
        {{ t('header.renameTask') }}
      </MenuItem>
      <MenuItem @click="forkSession">
        {{ t('header.forkSession') }}
      </MenuItem>
      <MenuItem danger @click="startArchive">{{ t('header.archiveTask') }}</MenuItem>
      <MenuItem separator />
      <!-- File / system actions -->
      <MenuItem @click="openInFileManager">
        {{ openInFileManagerLabel }}
      </MenuItem>
      <MenuItem @click="copyWorkspacePath">
        {{ copiedPath ? t('header.copied') : t('header.copyWorkspacePath') }}
      </MenuItem>
      <MenuItem v-if="sessionDir" @click="copySessionPath">
        {{ copiedSessionPath ? t('header.copied') : t('header.copySessionPath') }}
      </MenuItem>
      <MenuItem @click="copySessionId">
        {{ copiedId ? t('header.copied') : t('header.copySessionId') }}
      </MenuItem>
    </Menu>

    <div class="ch-spacer" />

    <!-- Git branch + status — plain text with semantic colors. Renders for any
         git repo, even a detached HEAD (empty branch → "detached" label), so the
         diff counter below is never hidden just because there's no branch name. -->
    <button
      v-if="isGitRepo"
      type="button"
      class="ch-git"
      @click="emit('openChanges')"
    >
      <span
        class="ch-branch"
        :class="{ 'ch-detached': !branch }"
      >
        {{ branch || t('header.detached') }}
      </span>
      <span v-if="ahead > 0 || behind > 0" class="ch-pill ch-sync-pill">
        <span v-if="ahead > 0" class="ch-ahead">↑{{ ahead }}</span>
        <span v-if="behind > 0" class="ch-behind">↓{{ behind }}</span>
      </span>
      <span v-if="hasLineStats" class="ch-pill ch-diff-pill">
        <span v-if="adds > 0" class="ch-add">+{{ adds }}</span>
        <span v-if="dels > 0" class="ch-del">-{{ dels }}</span>
      </span>
    </button>

    <!-- GitHub PR status -->
    <button
      v-if="pr"
      type="button"
      class="ch-pill ch-pr"
      :class="`pr-${pr.state}`"
      @click="pr && emit('openPr', pr.url)"
    >
      <Icon name="git-pull-request" size="sm" />
      <span>PR #{{ pr.number }} · {{ pr.state }}</span>
    </button>

    <!-- Rename dialog -->
    <RenameDialog
      :open="renameDialogOpen"
      :current-title="sessionTitle ?? ''"
      @update:open="renameDialogOpen = $event"
      @confirm="onRenameConfirm"
    />
  </header>
</template>

<style scoped>
.chat-header {
  flex: none;
  display: flex;
  align-items: center;
  gap: 14px;
  height: 48px;
  padding: 0 16px;
  border-bottom: 1px solid var(--color-line);
  background: var(--color-bg);
  font-family: var(--font-ui);
  min-width: 0;
}
/* macOS desktop: the window has a hidden title bar, so the conversation header
   doubles as a window-drag region. Interactive controls opt out with no-drag. */
.chat-header.macos-desktop {
  -webkit-app-region: drag;
}
.chat-header.macos-desktop button,
.chat-header.macos-desktop input {
  -webkit-app-region: no-drag;
}
/* When the sidebar is collapsed on macOS, the chat header is the leftmost
   element and must clear the traffic lights. */
.chat-header.macos-desktop.sidebar-hidden {
  padding-left: 80px;
}

.ch-sidebar-toggle,
.ch-new-chat {
  flex: none;
}

.ch-id { display: flex; align-items: center; gap: 6px; min-width: 0; flex: none; max-width: 46%; }
.ch-ws { color: var(--color-text-muted); font-size: var(--text-base); font-weight: var(--weight-medium); flex: none; }
.ch-sep { color: var(--color-text-faint); flex: none; }
.ch-ses {
  color: var(--color-text);
  font-size: var(--text-base);
  font-weight: var(--weight-medium);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.ch-git {
  display: flex;
  align-items: center;
  gap: 4px;
  border: none;
  background: transparent;
  padding: 0;
  color: var(--muted);
  font-family: var(--mono);
  font-size: calc(var(--ui-font-size) - 2px);
  flex: 0 1 auto;
  max-width: none;
  min-width: 0;
  cursor: pointer;
}
.ch-git:hover .ch-branch { color: var(--color-text); }
.ch-branch {
  color: var(--dim);
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  margin-right: 4px;
}
.ch-detached { color: var(--muted); font-style: italic; }
.ch-pill {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  padding: 1px 5px;
  border-radius: 999px;
  background: var(--panel);
  border: 1px solid var(--line);
  font-size: calc(var(--ui-font-size) - 3px);
}
.ch-sync-pill { border-color: var(--line); }
.ch-diff-pill { border-color: color-mix(in srgb, var(--color-success) 20%, var(--line)); }
.ch-ahead { color: var(--color-warning); flex: none; }
.ch-behind { color: var(--color-accent-hover); flex: none; }
.ch-add { color: var(--color-success); flex: none; }
.ch-del { color: var(--color-danger); flex: none; }
.ch-spacer { flex: 1; min-width: 0; }

/* Overflow "…" trigger — IconButton (md). The "open" state keeps the
   sunken highlight while the menu is showing. */
.ch-act-more.open { background: var(--color-surface-sunken); color: var(--color-text); }

/* GitHub PR badge — semantic state colors aligned with GitHub
   (open=green, merged=purple, closed=red, draft=gray). */
.ch-pr {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  height: 22px;
  padding: 0 9px;
  flex: none;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-full);
  background: var(--color-surface-sunken);
  color: var(--color-text-muted);
  font-size: var(--text-xs);
  font-weight: 500;
  cursor: pointer;
}
.ch-pr svg { flex: none; }
.ch-pr.pr-open { color: var(--color-success); border-color: var(--color-success-bd); background: var(--color-success-soft); }
.ch-pr.pr-merged { color: var(--color-done); border-color: var(--color-done-bd); background: var(--color-done-soft); }
.ch-pr.pr-closed { color: var(--color-danger); border-color: var(--color-danger-bd); background: var(--color-danger-soft); }
.ch-pr.pr-draft { color: var(--color-text-muted); border-color: var(--color-line-strong); background: var(--color-surface-sunken); }
.ch-pr:hover { border-color: var(--color-line-strong); }

/* Fixed more-menu, anchored to the kebab trigger. Surface / items come from
   the Menu + MenuItem primitives; only positioning stays here. */
.ch-menu {
  position: fixed;
  top: 0;
  left: 0;
  z-index: var(--z-dropdown);
}

/* On a narrow conversation column, the action labels collapse to icons. */
@media (max-width: 980px) {
  .ch-act-label { display: none; }
}
@media (max-width: 640px) {
  .chat-header { display: none; }
}
</style>
