<!-- apps/mirri-web/src/components/SessionRow.vue -->
<!-- A single session row: status dot + title + time + attention pill + quick-archive. -->
<!-- Inline rename (dblclick) and archive-confirm live here. -->
<!-- Full session actions (rename, fork, archive, copy, open-in-finder) live -->
<!-- in the ChatHeader `...` menu on the right — this row only needs a fast -->
<!-- archive shortcut on hover. -->
<script setup lang="ts">
import { nextTick, onUnmounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { Session } from '../types';
import Spinner from './ui/Spinner.vue';
import Badge from './ui/Badge.vue';
import IconButton from './ui/IconButton.vue';
import Icon from './ui/Icon.vue';
import Tooltip from './ui/Tooltip.vue';

const { t } = useI18n();

const props = withDefaults(
  defineProps<{
    session: Session;
    active: boolean;
    /** Pending permission requests waiting for the user's approval. */
    approvalCount?: number;
    /** Pending askUserQuestion prompts waiting for the user's answer. */
    questionCount?: number;
    /** A background turn finished here that the user hasn't opened — blue dot. */
    unread?: boolean;
  }>(),
  { approvalCount: 0, questionCount: 0, unread: false },
);

const emit = defineEmits<{
  select: [id: string];
  rename: [id: string, title: string];
  archive: [id: string];
}>();

// Inline rename
const renaming = ref(false);
const renameValue = ref('');
const renameInputRef = ref<HTMLInputElement | null>(null);
async function startRename(): Promise<void> {
  renaming.value = true;
  renameValue.value = props.session.title;
  await nextTick();
  try {
    renameInputRef.value?.focus();
    renameInputRef.value?.select();
  } catch {
    // jsdom may not implement focus/select
  }
}
function commitRename(): void {
  const newTitle = renameValue.value.trim();
  if (newTitle) emit('rename', props.session.id, newTitle);
  renaming.value = false;
}
function cancelRename(): void {
  renaming.value = false;
}

// Inline archive confirm: first click arms the button (turns it into a danger
// "confirm" state), second click emits archive. Any outside click / Esc
// disarms it. Replaces the old modal dialog for a faster, lower-friction flow.
const arming = ref(false);
const archiveBtnRef = ref<InstanceType<typeof IconButton> | null>(null);

function disarm(): void {
  arming.value = false;
  document.removeEventListener('mousedown', onDocClick);
}
function onDocClick(e: MouseEvent): void {
  if (!arming.value) return;
  const target = e.target as Node;
  if (archiveBtnRef.value?.el?.contains(target)) return;
  disarm();
}
function onKeydown(e: KeyboardEvent): void {
  if (e.key === 'Escape' && arming.value) disarm();
}
function armArchive(e: Event): void {
  e.stopPropagation();
  if (arming.value) {
    // Already armed — second click confirms.
    emit('archive', props.session.id);
    disarm();
    return;
  }
  arming.value = true;
  // Defer so the current click doesn't immediately disarm via onDocClick.
  setTimeout(() => document.addEventListener('mousedown', onDocClick), 0);
}
onUnmounted(() => {
  document.removeEventListener('mousedown', onDocClick);
  if (typeof window !== 'undefined') window.removeEventListener('keydown', onKeydown);
});
if (typeof window !== 'undefined') {
  window.addEventListener('keydown', onKeydown);
}
</script>

<template>
  <div class="se" :class="{ on: active }" @click="emit('select', session.id)" @mouseleave="disarm">
    <div class="row">
      <!-- Leading status slot (in the gutter left of the title): a spinner
           while the session runs, otherwise an unread blue dot. Fixed width
           so the title start never shifts. -->
      <span class="lead" aria-hidden="true">
        <Spinner v-if="session.busy" size="sm" />
        <span v-else-if="unread" class="unread-dot" />
      </span>

      <div class="left">
        <!-- Inline rename input -->
        <input
          v-if="renaming"
          ref="renameInputRef"
          v-model="renameValue"
          class="rename-input"
          @click.stop
          @keydown.enter.stop="commitRename"
          @keydown.esc.stop="cancelRename"
          @blur="commitRename"
        />
        <span v-else class="t" @dblclick.stop="startRename">{{ session.title }}</span>
      </div>

      <!-- Pending tags — coloured per kind, shown even when the row isn't
           active. "Answer" = an askUserQuestion is waiting; "Approve" = a
           permission request is waiting. The session's lifecycle status drives
           the same tags as a fallback for background sessions whose pending
           lists aren't loaded yet (status known, counts not). -->
      <Tooltip :text="t('workspace.awaitingAnswerTitle')">
        <Badge
          v-if="!renaming && (questionCount > 0 || session.pendingInteraction === 'question')"
          variant="info"
          size="sm"
        >
          {{ t('workspace.awaitingAnswer') }}
        </Badge>
      </Tooltip>
      <Tooltip :text="t('workspace.awaitingPermissionTitle')">
        <Badge
          v-if="!renaming && (approvalCount > 0 || session.pendingInteraction === 'approval')"
          variant="warning"
          size="sm"
        >
          {{ t('workspace.awaitingPermission') }}
        </Badge>
      </Tooltip>
      <!-- Aborted: a distinct, low-key error tag (not collapsed into idle). -->
      <Tooltip :text="t('workspace.abortedTitle')">
        <Badge
          v-if="!renaming && !session.busy && session.pendingInteraction !== 'question' && session.pendingInteraction !== 'approval' && questionCount === 0 && approvalCount === 0 && (session.lastTurnReason === 'cancelled' || session.lastTurnReason === 'failed')"
          variant="danger"
          size="sm"
        >
          {{ t('workspace.aborted') }}
        </Badge>
      </Tooltip>

      <!-- Trailing action slot: the relative time and the archive icon share one
           grid cell and swap via `visibility` (never display:none), so the slot
           width is identical in hover and rest. The badges and title therefore
           don't reflow on hover — see design-system §07 "Session row".
           Full session actions (rename, fork, copy, open-in-finder) live in the
           ChatHeader `...` menu; this row exposes only a fast archive shortcut.
           Click once to arm (danger state), click again to confirm — no modal. -->
      <span class="act">
        <span class="ts">{{ session.time }}</span>
        <Tooltip v-if="!renaming" :text="arming ? t('sidebar.archiveConfirmInline') : t('sidebar.archive')">
          <IconButton
            ref="archiveBtnRef"
            class="arc"
            :class="{ armed: arming }"
            size="sm"
            :label="t('sidebar.archive')"
            @click.stop="armArchive"
          >
            <Icon :name="arming ? 'check' : 'archive'" size="sm" />
          </IconButton>
        </Tooltip>
      </span>
    </div>
  </div>
</template>

<style scoped>
.se {
  /* --sb-* vars come from .side in Sidebar.vue: the title starts at
     --sb-pad-x + --sb-gutter + --sb-gap, exactly under the workspace name.
     The row is an inset pill: a 6px horizontal margin + 10px padding lands the
     leading icon at --sb-pad-x (16px), aligned with the workspace header. */
  display: block;
  margin: 0;
  padding: var(--space-1) var(--space-2);
  border-radius: var(--radius-md);
  font-family: var(--font-ui);
  color: var(--color-text);
  cursor: pointer;
  position: relative;
}
.se:hover { background: var(--color-surface-sunken); color: var(--color-text); }
.se.on {
  background: var(--color-accent-soft);
  color: var(--color-accent-hover);
  box-shadow: inset 0 0 0 1px var(--color-accent-bd);
}

.row {
  display: flex;
  align-items: center;
  gap: var(--sb-gap, 2px);
  min-width: 0;
  /* Floor the row at the hover-archive height (IconButton sm = 26px) so swapping
     the timestamp for the archive icon on hover doesn't grow the row. */
  min-height: 26px;
}

.left {
  display: flex;
  align-items: center;
  flex: 1;
  min-width: 0;
}

/* Leading status slot — mirrors the workspace header's icon slot (so the title
   aligns under the workspace name) AND carries the running spinner / unread dot.
   Fixed width keeps the title start fixed whether or not an indicator shows. */
.lead {
  width: var(--sb-gutter, 16px);
  flex: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}
.unread-dot {
  width: 7px;
  height: 7px;
  border-radius: var(--radius-full);
  background: var(--color-accent);
}

.t {
  color: inherit;
  font-size: var(--ui-font-size);
  font-weight: var(--weight-regular);
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.ts {
  color: var(--color-text-faint);
  font-size: var(--text-xs);
  font-family: var(--font-mono);
}

/* Trailing action slot: time and archive icon share one grid cell
   (grid-area:1/1). Both stay in the layout and swap via `visibility` (never
   display:none), so the slot width = max(time width, IconButton sm 26px) is
   identical in hover and rest — the badges and title don't reflow, eliminating
   hover jitter. `.act .arc` out-specificities IconButton's own display so the
   hidden default wins. */
.act {
  display: inline-grid;
  flex: none;
  align-items: center;
  justify-items: center;
}
.act .ts,
.act .arc { grid-area: 1 / 1; }
.act .arc { visibility: hidden; }
.se:hover .act .arc,
.act .arc.armed { visibility: visible; }
.se:hover .act .ts { visibility: hidden; }
.act:has(.arc.armed) .ts { visibility: hidden; }
/* Armed state — danger background so the user sees the confirm intent. */
.arc.armed {
  color: var(--color-danger, #ef4444);
  background: var(--color-danger-soft, rgba(239, 68, 68, 0.12));
}

.rename-input {
  flex: 1;
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-text);
  background: var(--color-bg);
  border: 1px solid var(--color-accent);
  border-radius: var(--radius-xs);
  padding: 1px 4px;
  outline: none;
  min-width: 0;
}

.sessions .se {
  margin: 0;
  border-radius: var(--radius-md);
  /* Trim the row padding by the inset margin so the title still starts at the
     same x as the workspace name (whose header has no inset). */
  padding: var(--space-1) calc(var(--sb-pad-x, 12px) - var(--space-2));
}
.sessions .se:hover { background: var(--panel2); }
.sessions .se.on {
  background: var(--color-accent-soft);
  box-shadow: inset 0 0 0 1px var(--color-accent-bd);
}
.sessions .se .rename-input { border-radius: var(--radius-sm); font-family: var(--sans); }
.sessions .se .arc { border-radius: var(--radius-sm); }
</style>
