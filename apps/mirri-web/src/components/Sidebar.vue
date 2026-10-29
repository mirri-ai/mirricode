<!-- apps/mirri-web/src/components/Sidebar.vue -->
<!-- Unified sidebar: session groups with collapsible workspace headers.
     The old workspace rail and workspace tabs have been removed;
     workspace switching, folding and renaming all live in the group header. -->
<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { serverEndpointLabel } from '../api/config';
import {
  fetchDevBackendState,
  initialDevBackendState,
  shortOrigin,
  switchDevBackend,
  type BackendName,
  type DevBackendState,
} from '../api/devBackend';
import { copyTextToClipboard } from '../lib/clipboard';
import {
  loadCollapsedWorkspaces,
  saveCollapsedWorkspaces,
} from '../lib/storage';
import { moveInOrder, type DropPosition, type WorkspaceSortMode } from '../lib/workspaceOrder';
import type { Session, WorkspaceGroup as WorkspaceGroupType, WorkspaceView } from '../types';
import SearchSessionsDialog from './dialogs/SearchSessionsDialog.vue';
import WorkspaceGroup from './WorkspaceGroup.vue';
import { isMacosDesktop } from '../lib/desktopFlag';
import IconButton from './ui/IconButton.vue';
import Icon from './ui/Icon.vue';
import Menu from './ui/Menu.vue';
import MenuItem from './ui/MenuItem.vue';
import Pill from './ui/Pill.vue';
import { useDeleteWorkspaceChoice } from '../composables/useDeleteWorkspaceChoice';
import type { WorkspaceDeleteMode } from '../api/types';

const { t } = useI18n();
const { confirmWorkspaceDeletion } = useDeleteWorkspaceChoice();

// Dev-only affordance: when the page is served by the Vite dev server, the
// logo turns yellow and a backend pill next to the brand shows which engine
// the dev proxy forwards to (v1 legacy server / v2 kap-server / v3 Rust core)
// — click it to switch without restarting Vite. In production this is all
// inert.
const isDev = import.meta.env.DEV;
const devBackend = ref<DevBackendState | null>(isDev ? initialDevBackendState() : null);
if (isDev) {
  onMounted(async () => {
    const live = await fetchDevBackendState();
    if (live) devBackend.value = live;
  });
}
// host:port of the server the dev proxy currently forwards to (fallback: the
// build-time label when the dev endpoints are unavailable).
const endpoint = computed(() => {
  if (!isDev) return '';
  const current = devBackend.value?.current;
  return current ? shortOrigin(current) : serverEndpointLabel();
});
const backendNames: BackendName[] = ['v3'];
function presetUrl(name: BackendName): string {
  const url = devBackend.value?.presets[name] ?? '';
  return url ? shortOrigin(url) : '';
}
function isCurrentBackend(name: BackendName): boolean {
  const state = devBackend.value;
  return state !== null && state.current === state.presets[name];
}

const props = withDefaults(
  defineProps<{
    activeWorkspace: WorkspaceView | null;
    activeWorkspaceId: string | null;
    sessions: Session[];
    groups: WorkspaceGroupType[];
    activeId: string;
    /** Current workspace sort mode — drives the section-header sort button. */
    workspaceSortMode: WorkspaceSortMode;
    /** Backend engine generation from /meta — dev-only badge next to the brand. */
    backend?: 'v1' | 'v2' | 'v3';
    attentionBySession?: Record<string, number>;
    /** Per-session pending counts split by kind, for the coloured tags. */
    pendingBySession?: Record<string, { approvals: number; questions: number }>;
    unreadBySession?: Record<string, boolean>;
    /** Width (px) of the session column, driven by the App resize handle. */
    colWidth?: number;
    /** Workspace ids whose manual session refresh is in flight — drives each
     *  WorkspaceGroup's refresh spinner. */
    refreshingIds?: Set<string>;
    /** Interrupted-task recovery (D.3): per-workspace interruption counts,
     *  keyed by the workspace registry id — feeds each group header chip. */
    interruptedCountByWorkspace?: Record<string, number>;
  }>(),
  {
    activeWorkspace: null,
    activeWorkspaceId: null,
    backend: 'v1',
    attentionBySession: () => ({}),
    pendingBySession: () => ({}),
    unreadBySession: () => ({}),
    colWidth: 220,
    refreshingIds: () => new Set<string>(),
    interruptedCountByWorkspace: () => ({}),
  },
);

const emit = defineEmits<{
  select: [sessionId: string];
  create: [];
  createInWorkspace: [workspaceId: string];
  selectWorkspace: [workspaceId: string];
  addWorkspace: [];
  rename: [id: string, title: string];
  archive: [id: string];
  deleteWorkspace: [id: string, mode: WorkspaceDeleteMode];
  reorderWorkspaces: [ids: string[]];
  setWorkspaceSortMode: [mode: WorkspaceSortMode];
  loadMoreSessions: [workspaceId: string];
  loadAllSessions: [];
  refreshWorkspace: [workspaceId: string];
  openSettings: [];
  /** Open the settings overlay directly on the tasks overview tab (B7). */
  openTasks: [];
  collapse: [];
}>();

// ---------------------------------------------------------------------------
// Session search dialog (Spotlight-style; filters title + last prompt)
// ---------------------------------------------------------------------------
const showSearch = ref(false);
const sessionSearchShortcut = isAppleShortcutPlatform() ? '⌘K' : 'Ctrl K';

function openSearch(): void {
  // Sessions are loaded per-workspace (first page only); lazily drain the rest
  // so the dialog's client-side filter covers everything.
  emit('loadAllSessions');
  showSearch.value = true;
}

function onSearchKeydown(e: KeyboardEvent): void {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
    e.preventDefault();
    openSearch();
  }
}

onMounted(() => window.addEventListener('keydown', onSearchKeydown));
onBeforeUnmount(() => window.removeEventListener('keydown', onSearchKeydown));

function isAppleShortcutPlatform(): boolean {
  if (typeof navigator === 'undefined') return false;
  if (/Mac|iPod|iPhone|iPad/.test(navigator.platform)) return true;

  const userAgentData = (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData;
  return userAgentData?.platform === 'macOS' || userAgentData?.platform === 'iOS';
}

// Scroll-linked header seam: the .btn-wrap bottom border/shadow only appears
// once the session list has actually scrolled, so an unscrolled list shows no
// abrupt boundary.
const sessionsScrolled = ref(false);
function onSessionsScroll(e: Event): void {
  sessionsScrolled.value = (e.target as HTMLElement).scrollTop > 0;
}

// ---------------------------------------------------------------------------
// Collapse groups
// ---------------------------------------------------------------------------
const collapsedIds = ref<Set<string>>(new Set(loadCollapsedWorkspaces()));

function isCollapsed(id: string): boolean {
  return collapsedIds.value.has(id);
}

function toggleCollapse(id: string): void {
  const next = new Set(collapsedIds.value);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  collapsedIds.value = next;
  saveCollapsedWorkspaces(next);
}

function collapseAllWorkspaces(): void {
  const next = new Set(props.groups.map((g) => g.workspace.id));
  collapsedIds.value = next;
  saveCollapsedWorkspaces(next);
}

function expandAllWorkspaces(): void {
  const next = new Set<string>();
  collapsedIds.value = next;
  saveCollapsedWorkspaces(next);
}

// True when every workspace is collapsed — drives the single toggle button's
// icon (expand when fully collapsed, collapse otherwise) and action.
const allCollapsed = computed(
  () =>
    props.groups.length > 0 &&
    props.groups.every((g) => collapsedIds.value.has(g.workspace.id)),
);

// ---------------------------------------------------------------------------
// In-group expand / collapse (show-more pagination)
// ---------------------------------------------------------------------------
// Tracks which workspace groups are "expanded" past their first page. Ephemeral
// (not persisted): a refresh reloads only the first page, so everything starts
// collapsed. Loading more expands automatically; the user can collapse back to
// the first page without losing the already-loaded data.
const expandedIds = ref<Set<string>>(new Set());

function isExpanded(id: string): boolean {
  return expandedIds.value.has(id);
}

function toggleExpand(id: string): void {
  const next = new Set(expandedIds.value);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  expandedIds.value = next;
}

function onLoadMore(id: string): void {
  // Loading more should reveal the new rows immediately.
  if (!expandedIds.value.has(id)) {
    const next = new Set(expandedIds.value);
    next.add(id);
    expandedIds.value = next;
  }
  emit('loadMoreSessions', id);
}

// ---------------------------------------------------------------------------
// Workspace drag-to-reorder
// ---------------------------------------------------------------------------
// The header of each group is the drag handle (see WorkspaceGroup). We track
// which group is being dragged and where the insertion marker sits (before or
// after the group under the pointer), then on drop we emit the new id order
// upward — the parent persists it and the computed `groups` re-sorts. Using the
// pointer's position within the target (top half = before, bottom half = after)
// is what lets a workspace be dropped at the very bottom of the list.
const draggingWsId = ref<string | null>(null);
const dragOver = ref<{ id: string; position: DropPosition } | null>(null);

function onWsDragstart(id: string): void {
  draggingWsId.value = id;
}

function onWsDragend(): void {
  draggingWsId.value = null;
  dragOver.value = null;
}

function dropPosition(event: DragEvent): DropPosition {
  const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
  return event.clientY < rect.top + rect.height / 2 ? 'before' : 'after';
}

function onGroupDragOver(event: DragEvent, targetId: string): void {
  if (draggingWsId.value === null || draggingWsId.value === targetId) return;
  event.preventDefault();
  if (event.dataTransfer) event.dataTransfer.dropEffect = 'move';
  dragOver.value = { id: targetId, position: dropPosition(event) };
}

function onGroupDrop(targetId: string): void {
  const fromId = draggingWsId.value;
  const position = dragOver.value?.id === targetId ? dragOver.value.position : 'before';
  dragOver.value = null;
  draggingWsId.value = null;
  if (!fromId || fromId === targetId) return;
  const next = moveInOrder(
    props.groups.map((g) => g.workspace.id),
    fromId,
    targetId,
    position,
  );
  emit('reorderWorkspaces', next);
}

function handleGhClick(wsId: string, e: MouseEvent): void {
  // Ignore clicks that land on the group's action buttons (refresh / kebab /
  // add); those have their own handlers and must not also toggle collapse.
  if ((e.target as Element).closest('.gh-more, .gh-refresh, .gh-add')) return;
  toggleCollapse(wsId);
}

function onSelectSession(sessionId: string): void {
  emit('select', sessionId);
}

// ---------------------------------------------------------------------------
// Workspace right-click menu (copy path, rename)
// ---------------------------------------------------------------------------
const ghMenuOpen = ref(false);
const ghMenuTarget = ref<WorkspaceView | null>(null);
const ghMenuStyle = ref<Record<string, string>>({});
const ghMenuRef = ref<InstanceType<typeof Menu> | null>(null);

function onGhMenuDocClick(e: MouseEvent): void {
  if (ghMenuRef.value?.el && !ghMenuRef.value.el.contains(e.target as Node)) {
    closeGhMenu();
  }
}

function openGhMenu(ws: WorkspaceView, e: MouseEvent): void {
  e.preventDefault();
  e.stopPropagation();
  ghMenuTarget.value = ws;
  ghMenuStyle.value = {
    top: `${e.clientY}px`,
    left: `${e.clientX}px`,
  };
  ghMenuOpen.value = true;
  document.addEventListener('mousedown', onGhMenuDocClick, true);
}

function closeGhMenu(): void {
  ghMenuOpen.value = false;
  document.removeEventListener('mousedown', onGhMenuDocClick, true);
  ghMenuTarget.value = null;
}

function copyPathFromMenu(): void {
  if (ghMenuTarget.value) {
    void copyTextToClipboard(ghMenuTarget.value.root);
  }
  closeGhMenu();
}

async function deleteFromMenu(): Promise<void> {
  const ws = ghMenuTarget.value;
  if (!ws) return;
  closeGhMenu();
  // The W6 three-choice: purge (delete the data) / remove (keep every byte,
  // re-adding the root restores) / cancel.
  const mode = await confirmWorkspaceDeletion({
    id: ws.id,
    name: ws.name,
    sessionCount: ws.sessionCount,
  });
  if (mode !== null) {
    emit('deleteWorkspace', ws.id, mode);
  }
}

// ---------------------------------------------------------------------------
// Workspace inline more-menu (kebab, hover-triggered). Rendered position:fixed
// and anchored to the ⋯ button so the scrolling session list can't clip it.
// It stays open on scroll (so a streaming turn doesn't dismiss it) and closes
// on outside-click or window resize.
// ---------------------------------------------------------------------------
const wsMenuOpenId = ref<string | null>(null);
const wsMenuTarget = ref<WorkspaceView | null>(null);
const wsMenuStyle = ref<Record<string, string>>({});
const wsMenuRef = ref<InstanceType<typeof Menu> | null>(null);

function onWsMenuDocClick(e: MouseEvent): void {
  const target = e.target as Element;
  if (target.closest('.gh-more') || target.closest('.ws-menu')) return;
  closeWsMenu();
}

async function toggleWsMenu(ws: WorkspaceView, e: MouseEvent): Promise<void> {
  if (wsMenuOpenId.value === ws.id) {
    closeWsMenu();
    return;
  }
  const btn = e.currentTarget as HTMLElement;
  wsMenuTarget.value = ws;
  wsMenuOpenId.value = ws.id;
  document.addEventListener('mousedown', onWsMenuDocClick);
  window.addEventListener('resize', closeWsMenu);
  await nextTick();
  const menu = wsMenuRef.value?.el;
  const r = btn.getBoundingClientRect();
  const gap = 4;
  const margin = 8;
  const menuH = menu?.offsetHeight ?? 0;
  const menuW = menu?.offsetWidth ?? 0;
  let top = r.bottom + gap;
  if (top + menuH > window.innerHeight - margin) {
    top = Math.max(margin, r.top - menuH - gap);
  }
  let left = r.right - menuW;
  if (left < margin) left = margin;
  wsMenuStyle.value = {
    top: `${Math.round(top)}px`,
    left: `${Math.round(left)}px`,
  };
}

function closeWsMenu(): void {
  wsMenuOpenId.value = null;
  wsMenuTarget.value = null;
  document.removeEventListener('mousedown', onWsMenuDocClick);
  window.removeEventListener('resize', closeWsMenu);
}

function copyWsPath(ws: WorkspaceView): void {
  void copyTextToClipboard(ws.root);
  closeWsMenu();
}

async function deleteWs(ws: WorkspaceView): Promise<void> {
  closeWsMenu();
  // The W6 three-choice: purge (delete the data) / remove (keep every byte,
  // re-adding the root restores) / cancel.
  const mode = await confirmWorkspaceDeletion({
    id: ws.id,
    name: ws.name,
    sessionCount: ws.sessionCount,
  });
  if (mode !== null) {
    emit('deleteWorkspace', ws.id, mode);
  }
}

// ---------------------------------------------------------------------------
// Workspace section overflow menu (the ⋯ in the WORKSPACES header). Holds the
// sort mode and the "show paths" toggle as text items with a check mark for the
// active one. Anchored to the trigger via position:fixed so the scrolling list
// can't clip it.
// ---------------------------------------------------------------------------
const sectionMenuOpen = ref(false);
const sectionMenuStyle = ref<Record<string, string>>({});
const sectionMenuRef = ref<InstanceType<typeof Menu> | null>(null);

function onSectionMenuDocClick(e: MouseEvent): void {
  const target = e.target as Element;
  if (target.closest('.side-section-kebab') || target.closest('.section-menu')) return;
  closeSectionMenu();
}

async function toggleSectionMenu(e: MouseEvent): Promise<void> {
  if (sectionMenuOpen.value) {
    closeSectionMenu();
    return;
  }
  const btn = e.currentTarget as HTMLElement;
  sectionMenuOpen.value = true;
  document.addEventListener('mousedown', onSectionMenuDocClick);
  window.addEventListener('resize', closeSectionMenu);
  await nextTick();
  const menu = sectionMenuRef.value?.el;
  const r = btn.getBoundingClientRect();
  const gap = 4;
  const margin = 8;
  const menuH = menu?.offsetHeight ?? 0;
  const menuW = menu?.offsetWidth ?? 0;
  let top = r.bottom + gap;
  if (top + menuH > window.innerHeight - margin) {
    top = Math.max(margin, r.top - menuH - gap);
  }
  let left = r.right - menuW;
  if (left < margin) left = margin;
  sectionMenuStyle.value = {
    top: `${Math.round(top)}px`,
    left: `${Math.round(left)}px`,
  };
}

function closeSectionMenu(): void {
  sectionMenuOpen.value = false;
  document.removeEventListener('mousedown', onSectionMenuDocClick);
  window.removeEventListener('resize', closeSectionMenu);
}

function chooseSortMode(mode: WorkspaceSortMode): void {
  emit('setWorkspaceSortMode', mode);
  closeSectionMenu();
}

// ---------------------------------------------------------------------------
// Dev backend switcher menu (the pill next to the brand). Dev-only: repoints
// the Vite dev proxy at the other engine, then reloads so every client state
// (REST, WS, /meta) re-initializes against the new backend.
// ---------------------------------------------------------------------------
const backendMenuOpen = ref(false);
const backendMenuStyle = ref<Record<string, string>>({});
const backendMenuRef = ref<InstanceType<typeof Menu> | null>(null);

function onBackendMenuDocClick(e: MouseEvent): void {
  const target = e.target as Element;
  if (target.closest('.ch-backend') || target.closest('.backend-menu')) return;
  closeBackendMenu();
}

async function toggleBackendMenu(e: MouseEvent): Promise<void> {
  if (devBackend.value === null) return;
  if (backendMenuOpen.value) {
    closeBackendMenu();
    return;
  }
  const btn = e.currentTarget as HTMLElement;
  backendMenuOpen.value = true;
  document.addEventListener('mousedown', onBackendMenuDocClick);
  window.addEventListener('resize', closeBackendMenu);
  await nextTick();
  const menu = backendMenuRef.value?.el;
  const r = btn.getBoundingClientRect();
  const gap = 4;
  const margin = 8;
  const menuH = menu?.offsetHeight ?? 0;
  let top = r.bottom + gap;
  if (top + menuH > window.innerHeight - margin) {
    top = Math.max(margin, r.top - menuH - gap);
  }
  backendMenuStyle.value = {
    top: `${Math.round(top)}px`,
    left: `${Math.round(Math.max(margin, r.left))}px`,
  };
}

function closeBackendMenu(): void {
  backendMenuOpen.value = false;
  document.removeEventListener('mousedown', onBackendMenuDocClick);
  window.removeEventListener('resize', closeBackendMenu);
}

async function chooseBackend(name: BackendName): Promise<void> {
  if (isCurrentBackend(name)) {
    closeBackendMenu();
    return;
  }
  const next = await switchDevBackend(name);
  if (next === null) {
    console.warn('[mirri-web] dev backend switch failed:', name);
    closeBackendMenu();
    return;
  }
  // Full reload: every client channel (REST base state, WS, /meta) must
  // re-initialize against the new backend — a soft swap would leave stale
  // session streams subscribed through the old target.
  window.location.reload();
}

onBeforeUnmount(() => {
  document.removeEventListener('mousedown', onGhMenuDocClick, true);
  document.removeEventListener('mousedown', onWsMenuDocClick);
  document.removeEventListener('mousedown', onSectionMenuDocClick);
  document.removeEventListener('mousedown', onBackendMenuDocClick);
  window.removeEventListener('resize', closeWsMenu);
  window.removeEventListener('resize', closeSectionMenu);
  window.removeEventListener('resize', closeBackendMenu);
});

// Temporarily hide the new-workspace button while we evaluate the entry point.
const showNewWorkspaceButton = false;
</script>

<template>
  <aside class="side" :class="{ 'macos-desktop': isMacosDesktop }">
    <!-- Session column -->
    <div class="col" :style="{ width: colWidth + 'px' }">
      <!-- Header: sidebar controls (48px, matches ChatHeader height). The
           collapse button sits leftmost (next to the macOS traffic lights);
           tasks + settings stay on the right. -->
      <div class="ch">
        <IconButton
          class="ch-collapse-btn"
          size="sm"
          :label="t('sidebar.collapseSidebar')"
          @click.stop="emit('collapse')"
        >
          <Icon name="sidebar-close" />
        </IconButton>
        <!-- Dev-only backend pill: shows which engine the Vite dev proxy
             forwards to (v1 / v2 / v3) and opens the switcher menu. Absent in
             production builds. -->
        <Pill
          v-if="isDev"
          class="ch-backend"
          :clickable="devBackend !== null"
          :title="t('sidebar.backendTitle', { backend, endpoint })"
          @click="toggleBackendMenu"
        >
          <span class="ch-backend-kind" :class="`is-${backend}`">{{ backend }}</span>
          <span class="ch-backend-ep"> · {{ endpoint }}</span>
          <Icon v-if="devBackend !== null" name="chevron-down" size="sm" />
        </Pill>
        <IconButton
          class="ch-tasks-btn"
          size="sm"
          :label="t('sidebar.openTasks')"
          @click.stop="emit('openTasks')"
        >
          <Icon name="list" />
        </IconButton>
        <IconButton
          class="ch-settings-btn"
          size="sm"
          :label="t('settings.title')"
          @click.stop="emit('openSettings')"
        >
          <Icon name="settings" />
        </IconButton>
      </div>

      <!-- Session search — opens the Spotlight-style search dialog -->
      <button class="search" type="button" @click="openSearch">
        <Icon class="search-icon" name="search" />
        <span class="search-input">{{ t('sidebar.searchShortcut', { shortcut: sessionSearchShortcut }) }}</span>
      </button>

      <!-- New chat + new workspace buttons -->
      <div class="btn-wrap" :class="{ 'btn-wrap--scrolled': sessionsScrolled }">
        <button class="btn-new-chat" type="button" @click.stop="emit('create')">
          <Icon name="chat-new" />
          <span>{{ t('sidebar.newChat') }}</span>
        </button>
        <IconButton
          v-if="showNewWorkspaceButton"
          size="sm"
          :label="t('sidebar.newWorkspace')"
          @click.stop="emit('addWorkspace')"
        >
          <Icon name="folder" />
        </IconButton>
      </div>

      <!-- Session list — grouped by workspace -->
      <div class="sessions" @scroll="onSessionsScroll">
        <!-- Empty state — only when no workspace is registered at all; empty
             workspaces still render their group header (with the + button). -->
        <div v-if="groups.length === 0" class="empty">
          {{ t('workspace.noWorkspace') }}
        </div>

        <template v-else>
          <div class="side-section-label">
            <span class="side-section-title">{{ t('sidebar.workspaces') }}</span>
            <div class="side-section-actions">
              <IconButton
                class="side-section-toggle"
                size="sm"
                :label="allCollapsed ? t('sidebar.expandAll') : t('sidebar.collapseAll')"
                @click.stop="allCollapsed ? expandAllWorkspaces() : collapseAllWorkspaces()"
              >
                <Icon v-if="allCollapsed" name="expand" />
                <Icon v-else name="collapse" />
              </IconButton>
              <IconButton
                class="side-section-toggle side-section-kebab"
                size="sm"
                :label="t('sidebar.options')"
                aria-haspopup="menu"
                :aria-expanded="sectionMenuOpen"
                @click.stop="toggleSectionMenu($event)"
              >
                <Icon name="dots-horizontal" />
              </IconButton>
            </div>
          </div>
          <div
            v-for="g in groups"
            :key="g.workspace.id"
            class="ws-drop-target"
            :class="{
              'drop-before': dragOver?.id === g.workspace.id && dragOver.position === 'before',
              'drop-after': dragOver?.id === g.workspace.id && dragOver.position === 'after',
            }"
            @dragover="onGroupDragOver($event, g.workspace.id)"
            @drop="onGroupDrop(g.workspace.id)"
          >
            <WorkspaceGroup
              :group="g"
              :active-workspace-id="activeWorkspaceId"
              :active-id="activeId"
              :pending-by-session="pendingBySession"
              :unread-by-session="unreadBySession"
              :ws-menu-open-id="wsMenuOpenId"
              :dragging="draggingWsId === g.workspace.id"
              :refreshing="refreshingIds.has(g.workspace.id)"
              :recovery-count="interruptedCountByWorkspace[g.workspace.id] ?? 0"
              :is-collapsed="isCollapsed"
              :is-expanded="isExpanded"
              @group-click="handleGhClick"
              @group-contextmenu="openGhMenu"
              @toggle-ws-menu="toggleWsMenu"
              @create-in-workspace="(id) => emit('createInWorkspace', id)"
              @select-session="onSelectSession"
              @rename-session="(id, title) => emit('rename', id, title)"
              @archive-session="(id) => emit('archive', id)"
              @load-more="onLoadMore"
              @toggle-expand="toggleExpand"
              @ws-dragstart="onWsDragstart"
              @ws-dragend="onWsDragend"
              @refresh-workspace="(id) => emit('refreshWorkspace', id)"
            />
          </div>
        </template>
      </div>
    </div>

    <!-- Workspace right-click menu (position:fixed) -->
    <Menu
      v-if="ghMenuOpen"
      ref="ghMenuRef"
      class="gh-menu"
      :style="ghMenuStyle"
      @click.stop
    >
      <MenuItem @click="copyPathFromMenu">{{ t('sidebar.copyPath') }}</MenuItem>
      <MenuItem danger @click="deleteFromMenu">{{ t('sidebar.removeWorkspace') }}</MenuItem>
    </Menu>

    <!-- Workspace kebab menu (position:fixed, anchored to the ⋯ button so the
         scrolling session list cannot clip it) -->
    <Menu
      v-if="wsMenuOpenId !== null && wsMenuTarget"
      ref="wsMenuRef"
      class="ws-menu"
      :style="wsMenuStyle"
      @click.stop
    >
      <MenuItem @click="copyWsPath(wsMenuTarget)">{{ t('sidebar.copyPath') }}</MenuItem>
      <MenuItem danger @click="deleteWs(wsMenuTarget)">{{ t('sidebar.removeWorkspace') }}</MenuItem>
    </Menu>
    <!-- Workspace sort menu (position:fixed, anchored to the sort button) -->
    <Menu
      v-if="sectionMenuOpen"
      ref="sectionMenuRef"
      class="section-menu"
      :style="sectionMenuStyle"
      @click.stop
    >
      <MenuItem @click="chooseSortMode('manual')">
        <span class="section-menu-check">
          <Icon v-if="workspaceSortMode === 'manual'" name="check" size="sm" />
        </span>
        {{ t('sidebar.sortManual') }}
      </MenuItem>
      <MenuItem @click="chooseSortMode('recent')">
        <span class="section-menu-check">
          <Icon v-if="workspaceSortMode === 'recent'" name="check" size="sm" />
        </span>
        {{ t('sidebar.sortRecent') }}
      </MenuItem>
    </Menu>
    <!-- Dev backend switcher menu (position:fixed, anchored to the brand pill) -->
    <Menu
      v-if="backendMenuOpen"
      ref="backendMenuRef"
      class="backend-menu"
      :style="backendMenuStyle"
      @click.stop
    >
      <MenuItem v-for="name in backendNames" :key="name" @click="chooseBackend(name)">
        <span class="section-menu-check">
          <Icon v-if="isCurrentBackend(name)" name="check" size="sm" />
        </span>
        <span class="backend-menu-name">{{ name }}</span>
        <span class="backend-menu-url">{{ presetUrl(name) }}</span>
      </MenuItem>
    </Menu>
    <!-- Session search dialog (Cmd/Ctrl+K) -->
    <SearchSessionsDialog
      v-if="showSearch"
      :sessions="sessions"
      :active-id="activeId"
      @select="onSelectSession"
      @close="showSearch = false"
    />
  </aside>
</template>

<style scoped>
.side {
  border-right: 1px solid var(--line);
  background: var(--panel);
  display: flex;
  flex-direction: row;
  min-width: 0;
  height: 100%;
  /* Alignment contract, inherited by SessionRow and the de-terminalization
     rules in style.css: text in the workspace header, the path line and session
     rows all starts at --sb-pad-x + --sb-gutter + --sb-gap from the sidebar edge. */
  --sb-pad-x: var(--space-4);  /* row horizontal padding */
  --sb-gutter: 20px;           /* leading icon slot (14px folder icon + 6px margin) */
  --sb-gap: var(--space-2);    /* gap between the icon slot and the text */
  /* Sidebar stays one step above compact UI chrome, but still follows the
     user-controlled font-size preference. */
  --ui-font-size: var(--sidebar-ui-font-size);
}

/* Session column. Width is set inline from the App resize handle. */
.col {
  flex: none;
  min-width: 0;
  display: flex;
  flex-direction: column;
  min-height: 0;
  width: 100%;
  container-type: inline-size;
  container-name: sidebar-col;
}

/* Header: sidebar controls (48px, matches ChatHeader height). The collapse
   button leads on the left; the remaining controls sit on the right. */
.ch {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 8px;
  height: 48px;
  flex: none;
  padding: 0 var(--space-3);
  width: 100%;
  box-sizing: border-box;
  border-bottom: 1px solid var(--color-line);
}
.ch-collapse-btn {
  margin-right: auto; /* collapse left, everything else right */
}
/* macOS desktop: the window uses a hidden title bar, so the traffic lights float
   over the top-left of the sidebar. Push the header content right to clear them,
   and turn the whole header into the window-drag region — matching the chat
   header. The action buttons opt out with no-drag so they stay clickable. */
.side.macos-desktop .ch {
  -webkit-app-region: drag;
  padding-left: var(--macos-traffic-inset);
}
.side.macos-desktop .ch button {
  -webkit-app-region: no-drag;
}
/* Dev-only backend pill in the header: shows which engine the dev proxy
   forwards to (v1 / v2 / v3) and opens the switcher menu. v2 is accent-colored
   so the engines read differently at a glance. */
.ch-backend {
  flex: none;
  min-width: 0;
}
.ch-backend-kind {
  font-family: var(--mono);
  font-weight: 500;
  color: var(--color-text-muted);
}
.ch-backend-kind.is-v2 {
  color: var(--color-accent);
}
.ch-backend-ep {
  font-family: var(--mono);
  color: var(--color-text-faint);
  overflow: hidden;
  text-overflow: ellipsis;
}

/* In narrow sidebars the pill's endpoint drops out (the v1/v2/v3 kind +
   chevron stay — the full target is one tooltip away). */
@container sidebar-col (max-width: 320px) {
  .ch-backend-ep { display: none; }
}

/* Action buttons */
 .btn-wrap {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 var(--space-2) var(--space-2);
  position: relative;
  z-index: 1;
  background: var(--panel);
  border-bottom: 1px solid transparent;
  transition: border-color var(--duration-base) var(--ease-out),
    box-shadow var(--duration-base) var(--ease-out);
}
.btn-wrap--scrolled {
  border-bottom-color: var(--line);
  box-shadow: var(--shadow-sm);
}
.btn-new-chat {
  display: flex;
  align-items: center;
  gap: 12px;
  flex: 1;
  min-width: 0;
  min-height: 26px;
  padding: var(--space-1) calc(var(--sb-pad-x) - var(--space-2));
  border: none;
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--color-text);
  font-family: var(--font-ui);
  font-size: var(--ui-font-size);
  cursor: pointer;
  text-align: left;
}
.btn-new-chat:hover { background: var(--color-surface-sunken); }
.btn-new-chat:focus-visible { outline: none; box-shadow: var(--p-focus-ring); }
.btn-new-chat svg { flex: none; width: 16px; height: 16px; }
.btn-new-chat span {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* Session search */
.search {
  display: flex;
  align-items: center;
  gap: 12px;
  min-height: 26px;
  margin: 0 var(--space-2) var(--space-2);
  padding: var(--space-1) calc(var(--sb-pad-x) - var(--space-2));
  border: none;
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--color-text);
  font: inherit;
  text-align: left;
  cursor: pointer;
}
.search:hover { background: var(--color-surface-sunken); }
.search:focus-visible {
  background: var(--color-surface-sunken);
  color: var(--color-text);
  outline: 2px solid var(--color-accent-bd);
  outline-offset: -2px;
}
.search-icon {
  flex: none;
}
.search-input {
  flex: 1;
  min-width: 0;
  color: var(--color-text);
  font-family: var(--mono);
  font-size: var(--ui-font-size);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* Sessions */
.sessions {
  flex: 1;
  overflow-y: auto;
  padding: 0 var(--space-2) var(--space-2);
  min-height: 0;
  scrollbar-width: thin;
  scrollbar-color: var(--line) transparent;
}
.sessions::-webkit-scrollbar { width: 4px; }
.sessions::-webkit-scrollbar-track { background: transparent; }
.sessions::-webkit-scrollbar-thumb {
  background: var(--line);
  border-radius: var(--radius-xs);
}
.sessions::-webkit-scrollbar-thumb:hover { background: var(--color-accent-bd); }

/* Section label — heads the workspace list below the action buttons. Aligns
   with the rows' leading inset (--sb-pad-x) so it reads as the list's title. */
.side-section-label {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 0 var(--space-3) var(--space-1) var(--space-2);
  font-size: var(--text-sm);
  font-weight: 500;
  letter-spacing: .08em;
  text-transform: uppercase;
  color: var(--faint);
  user-select: none;
}
.side-section-title {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.side-section-toggle {
  color: var(--faint);
  opacity: 0;
  transition: opacity var(--duration-base) var(--ease-out);
}
.side-section-label:hover .side-section-toggle,
.side-section-label:focus-within .side-section-toggle {
  opacity: 1;
}
.side-section-toggle:hover {
  color: var(--dim);
}
.side-section-toggle svg {
  width: 13px;
  height: 13px;
}
.side-section-actions {
  display: flex;
  align-items: center;
  gap: 2px;
}

/* Workspace drag-to-reorder: a line at the top (drop-before) or bottom
   (drop-after) of the group under the cursor marks where the dragged workspace
   will land. Inset shadows avoid layout shift. */
.ws-drop-target.drop-before { box-shadow: inset 0 2px 0 var(--color-accent); }
.ws-drop-target.drop-after { box-shadow: inset 0 -2px 0 var(--color-accent); }

.empty {
  padding: var(--space-6) var(--space-3);
  text-align: center;
  color: var(--faint);
  font-size: calc(var(--ui-font-size) - 3px);
  line-height: 1.6;
}

/* Workspace menus — surface + items come from Menu / MenuItem; only the
   fixed positioning stays here (anchored to the ⋯ trigger / cursor). */
.ws-menu,
.gh-menu,
.section-menu,
.backend-menu {
  position: fixed;
  top: 0;
  left: 0;
  z-index: var(--z-dropdown);
}

/* Check slot for the section overflow menu — fixed width so unchecked items
   keep their text aligned with the checked one. */
.section-menu-check {
  display: inline-flex;
  flex: none;
  width: 14px;
}

/* Backend switcher menu rows: mono engine name + muted preset URL. */
.backend-menu-name {
  font-family: var(--mono);
  font-weight: 500;
}
.backend-menu-url {
  margin-left: 8px;
  font-family: var(--mono);
  color: var(--color-text-muted);
}

</style>
