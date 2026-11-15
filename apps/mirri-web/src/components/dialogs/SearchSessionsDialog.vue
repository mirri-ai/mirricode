<!-- apps/mirri-web/src/components/dialogs/SearchSessionsDialog.vue -->
<!-- Spotlight-style session search. Empty query shows the recent list;
     a non-empty query goes to the server's global session search on the rust
     transport (session.search — title + message content across sessions,
     snippet centered on the match), falling back to the client-side title
     filter elsewhere. Each hit shows its workspace, the session title, and
     the snippet with the query highlighted. ↑/↓ to move, ↵ to open, Esc to
     close. -->
<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { Session } from '../../types';
import { getMirriWebApi } from '../../api';
import { isCompositionKeyEvent } from '../../lib/keyboard';
import { highlightHtml, snippet } from '../../lib/searchHighlight';
import {
  SESSION_SEARCH_PAGE_SIZE,
  isServerSearchableQuery,
  sessionSearchPageToRows,
  type SessionSearchRow,
} from '../../lib/sessionSearch';
import Dialog from '../ui/Dialog.vue';
import Icon from '../ui/Icon.vue';

const { t } = useI18n();

const props = defineProps<{
  sessions: Session[];
  activeId: string;
}>();

const emit = defineEmits<{
  select: [id: string];
  close: [];
  /** Quick pin/unpin from a result row — same semantics as the sidebar
   *  row's pin button (single click = colorless pin / unpin). Color
   *  editing stays in the sidebar's status-pin palette. */
  pin: [id: string];
  unpin: [id: string];
}>();

// Server-backed hits are lean views without the pin trio; resolve the
// pinned flag against the full session list so the quick toggle's state
// (and therefore its pin/unpin direction) is right on both paths.
const pinnedById = computed(() => new Map(props.sessions.map((s) => [s.id, s.pinned === true])));

function isPinned(id: string, fallback: boolean | undefined): boolean {
  if (fallback !== undefined) return fallback;
  return pinnedById.value.get(id) ?? false;
}

// The parent controls visibility with `v-if`, so the dialog is open whenever
// this component is mounted. Dialog owns focus trap, Esc/overlay close, and the
// close button; we forward its `close` event to the parent.
const open = ref(true);

const query = ref('');
const inputRef = ref<HTMLInputElement | null>(null);
const listRef = ref<HTMLElement | null>(null);

interface Hit {
  session: Session;
  /** Title matched the query (controls title highlighting). */
  inTitle: boolean;
  /** Workspace name matched the query (controls workspace highlighting). */
  inWorkspace: boolean;
  /** Snippet of lastPrompt to preview under the title (empty when absent). */
  snippetText: string;
}

const RESULT_CAP = 200;

// The client-side title/last-prompt filter (the empty-query recent list and
// the non-rust fallback path — the pre-existing behavior is unchanged).
const clientResults = computed<Hit[]>(() => {
  const q = query.value.trim().toLowerCase();
  const out: Hit[] = [];
  for (const s of props.sessions) {
    const title = s.title ?? '';
    const last = s.lastPrompt ?? '';
    const ws = s.workspaceName ?? '';
    const inTitle = q.length > 0 && title.toLowerCase().includes(q);
    const inLast = q.length > 0 && last.toLowerCase().includes(q);
    const inWorkspace = q.length > 0 && ws.toLowerCase().includes(q);
    // Empty query → show the full (recent) list; otherwise require a hit.
    if (q.length > 0 && !inTitle && !inLast && !inWorkspace) continue;
    out.push({
      session: s,
      inTitle,
      inWorkspace,
      // Preview the last prompt whenever available; when searching, anchor the
      // snippet on the match (no-ops to the head when the title matched only).
      snippetText: last ? snippet(last, query.value) : '',
    });
    if (out.length >= RESULT_CAP) break;
  }
  return out;
});

// The server-backed global search (the rust engine's `session.search`):
// non-empty queries go to the server when the transport offers it, with a
// debounce and a stale-response guard — an older page never overwrites a
// newer one, and a failed search degrades back to the client filter
// rather than blanking the list.
const serverResults = ref<SessionSearchRow[] | null>(null);
const searching = ref(false);
let searchSequence = 0;
let searchTimer: ReturnType<typeof setTimeout> | null = null;

const serverSearchAvailable = computed(() =>
  typeof getMirriWebApi().searchSessions === 'function',
);

async function runServerSearch(text: string, sequence: number): Promise<void> {
  try {
    const page = await getMirriWebApi().searchSessions!({
      query: text.trim(),
      pageSize: SESSION_SEARCH_PAGE_SIZE,
    });
    if (sequence !== searchSequence) return;
    serverResults.value = sessionSearchPageToRows(page);
  } catch {
    // A failed server search falls back to the client-side filter for this
    // query (the next keystroke retries the server).
    if (sequence === searchSequence) serverResults.value = null;
  } finally {
    if (sequence === searchSequence) searching.value = false;
  }
}

watch(query, (text) => {
  selectedIndex.value = 0;
  if (searchTimer !== null) {
    clearTimeout(searchTimer);
    searchTimer = null;
  }
  searchSequence += 1;
  searching.value = false;
  if (!serverSearchAvailable.value || !isServerSearchableQuery(text)) {
    serverResults.value = null;
    return;
  }
  searching.value = true;
  const sequence = searchSequence;
  searchTimer = setTimeout(() => {
    searchTimer = null;
    void runServerSearch(text, sequence);
  }, 200);
});

onBeforeUnmount(() => {
  if (searchTimer !== null) clearTimeout(searchTimer);
  searchSequence += 1;
});

const results = computed<Hit[]>(() =>
  serverResults.value !== null ? serverResults.value : clientResults.value,
);

const showSearching = computed(() => searching.value && results.value.length === 0);

const selectedIndex = ref(0);

function clampIndex(i: number): number {
  const len = results.value.length;
  if (len === 0) return 0;
  return Math.max(0, Math.min(len - 1, i));
}

async function scrollSelectedIntoView(): Promise<void> {
  await nextTick();
  const el = listRef.value?.querySelector<HTMLElement>('[aria-selected="true"]');
  el?.scrollIntoView({ block: 'nearest' });
}

function move(delta: number): void {
  selectedIndex.value = clampIndex(selectedIndex.value + delta);
  void scrollSelectedIntoView();
}

function openHit(id: string): void {
  emit('select', id);
  emit('close');
}

function openSelected(): void {
  const hit = results.value[selectedIndex.value];
  if (hit) openHit(hit.session.id);
}

/** Quick pin toggle on a result row — mirrors the sidebar row's pin button. */
function togglePin(hit: Hit): void {
  if (isPinned(hit.session.id, hit.session.pinned)) emit('unpin', hit.session.id);
  else emit('pin', hit.session.id);
}

function onKeydown(e: KeyboardEvent): void {
  if (isCompositionKeyEvent(e)) return;
  if (e.key === 'ArrowDown') {
    e.preventDefault();
    move(1);
  } else if (e.key === 'ArrowUp') {
    e.preventDefault();
    move(-1);
  } else if (e.key === 'Enter') {
    e.preventDefault();
    openSelected();
  }
  // Escape is intentionally left to bubble so Dialog closes the modal.
}

onMounted(() => {
  // Dialog also auto-focuses the first focusable element; this is a belt-and-
  // suspenders guarantee for the rare timing where it runs before mount.
  inputRef.value?.focus();
});
</script>

<template>
  <Dialog v-model:open="open" size="lg" height="fixed" :padded="false" @close="emit('close')">
    <template #head>
      <div class="sd-head">
        <Icon class="sd-search-icon" name="search" size="md" />
        <input
          ref="inputRef"
          v-model="query"
          class="sd-input"
          type="text"
          :placeholder="t('sidebar.searchPlaceholder')"
          :aria-label="t('sidebar.searchPlaceholder')"
          autocomplete="off"
          spellcheck="false"
          @keydown="onKeydown"
        />
      </div>
    </template>

    <div ref="listRef" class="sd-list" role="listbox">
      <template v-if="results.length > 0">
        <!-- The row is a div[role=option] (not a <button>) so it can nest
             the quick pin button without invalid button-in-button markup;
             keyboard flow is owned by the search input (↑↓/↵), the row
             only needs pointer selection + the aria-selected state. -->
        <div
          v-for="(hit, i) in results"
          :key="hit.session.id"
          class="sd-row"
          :class="{ on: i === selectedIndex, active: hit.session.id === activeId }"
          role="option"
          :aria-selected="i === selectedIndex"
          @click="openHit(hit.session.id)"
          @mousemove="selectedIndex = i"
        >
          <span class="sd-meta">
            <Icon class="sd-folder" name="folder-closed" size="sm" />
            <!-- eslint-disable-next-line vue/no-v-html -- highlightHtml escapes the source before injecting <mark>. -->
            <span
              class="sd-ws"
              v-html="highlightHtml(hit.session.workspaceName ?? hit.session.workspaceId ?? '', hit.inWorkspace ? query : '')"
            ></span>
            <span class="sd-time">{{ hit.session.time }}</span>
            <button
              type="button"
              class="sd-pin"
              :class="isPinned(hit.session.id, hit.session.pinned) ? ['pinned', `pin-${hit.session.pinColor ?? 'gray'}`] : undefined"
              :label="isPinned(hit.session.id, hit.session.pinned) ? t('sidebar.unpin') : t('sidebar.pin')"
              :aria-label="isPinned(hit.session.id, hit.session.pinned) ? t('sidebar.unpin') : t('sidebar.pin')"
              data-test-id="search-pin-btn"
              @click.stop="togglePin(hit)"
            >
              <Icon name="pushpin" size="sm" />
            </button>
          </span>
          <!-- eslint-disable-next-line vue/no-v-html -- highlightHtml escapes the source before injecting <mark>. -->
          <span class="sd-title" v-html="highlightHtml(hit.session.title, hit.inTitle ? query : '')"></span>
          <!-- eslint-disable-next-line vue/no-v-html -- highlightHtml escapes the source before injecting <mark>. -->
          <span
            v-if="hit.snippetText"
            class="sd-snippet"
            v-html="highlightHtml(hit.snippetText, query)"
          ></span>
        </div>
      </template>
      <div v-else class="sd-empty">
        {{ showSearching ? t('sidebar.searching') : t('sidebar.searchNoResults') }}
      </div>
    </div>

    <template #foot>
      <span class="sd-hint">{{ t('sidebar.searchHint') }}</span>
    </template>
  </Dialog>
</template>

<style scoped>
.sd-head {
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.sd-search-icon {
  flex: none;
  color: var(--color-text-muted);
}
.sd-input {
  flex: 1;
  min-width: 0;
  font-family: var(--font-ui);
  font-size: var(--text-lg);
  color: var(--color-text);
  background: none;
  border: none;
  outline: none;
  padding: var(--space-1) 0;
}
.sd-input::placeholder {
  color: var(--color-text-muted);
}

.sd-list {
  height: 420px;
  overflow-y: auto;
  padding: var(--space-1) var(--space-2);
}
.sd-row {
  display: flex;
  flex-direction: column;
  gap: 2px;
  width: 100%;
  padding: var(--space-2) var(--space-3);
  border: none;
  border-radius: var(--radius-md);
  background: none;
  cursor: pointer;
  text-align: left;
  font-family: var(--font-ui);
  color: var(--color-text);
}
.sd-row:hover,
.sd-row.on {
  background: var(--color-surface-sunken);
}
.sd-row.active .sd-title {
  color: var(--color-accent-hover);
}

.sd-meta {
  display: flex;
  align-items: center;
  gap: var(--space-1);
  min-width: 0;
  font-size: var(--text-xs);
  color: var(--color-text-muted);
}
.sd-folder {
  flex: none;
  color: var(--color-text-muted);
}
.sd-ws {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.sd-time {
  flex: none;
  font-family: var(--font-mono);
  color: var(--color-text-faint);
}

/* Quick pin button on a result row — same semantics as the sidebar row's
   pin button; muted at rest, tinted with the pin color while pinned. */
.sd-pin {
  flex: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 22px;
  padding: 0;
  border: none;
  border-radius: var(--radius-sm);
  background: none;
  color: var(--color-text-faint);
  cursor: pointer;
}
.sd-pin:hover {
  background: var(--color-surface-sunken);
  color: var(--color-text);
}
.sd-pin.pinned {
  color: var(--pin-color, var(--pin-gray));
}

.sd-title {
  min-width: 0;
  font-size: var(--text-base);
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.sd-snippet {
  min-width: 0;
  font-size: var(--text-sm);
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* v-html content is outside the scoped tree, so :deep is required to style the
   injected <mark>. */
.sd-title :deep(mark),
.sd-snippet :deep(mark) {
  background: var(--color-accent);
  color: var(--color-bg);
  font-weight: 600;
  border-radius: var(--radius-xs);
  padding: 0 2px;
}

.sd-empty {
  padding: var(--space-6) var(--space-4);
  text-align: center;
  color: var(--color-text-muted);
  font-size: var(--text-sm);
}
.sd-hint {
  font-size: var(--text-xs);
  color: var(--color-text-muted);
}
</style>
