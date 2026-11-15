<!-- apps/mirri-web/src/components/SessionRow.vue -->
<!-- A single session row: status dot + title + time + attention pill + quick-archive. -->
<!-- Inline rename (dblclick) and archive-confirm live here. -->
<!-- Full session actions (rename, fork, archive, copy, open-in-finder) live -->
<!-- in the ChatHeader `...` menu on the right — this row only needs a fast -->
<!-- archive shortcut on hover. -->
<script setup lang="ts">
import { computed, inject, nextTick, onUnmounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { SessionWorkFacts } from '../types';
import type { SortableSessionRow } from '../lib/sessionSorting';
import { formatTime } from '../composables/useMirriWebClient';
import { interruptedBadgeVisible } from '../lib/sessionRowRecovery';
import Spinner from './ui/Spinner.vue';
import Badge from './ui/Badge.vue';
import IconButton from './ui/IconButton.vue';
import Icon from './ui/Icon.vue';
import Tooltip from './ui/Tooltip.vue';
import { isCompositionKeyEvent } from '../lib/keyboard';

const { t } = useI18n();

const props = withDefaults(
  defineProps<{
    /** SDR-002: the sort projection {id, title, lastUsedAt, updatedAt} —
     *  position-relevant fields only. Badges and the relative time derive
     *  per-row below, so work-facts churn never re-sorts the list. */
    session: SortableSessionRow;
    active: boolean;
    /** Pending permission requests waiting for the user's approval. */
    approvalCount?: number;
    /** Pending askUserQuestion prompts waiting for the user's answer. */
    questionCount?: number;
    /** A background turn finished here that the user hasn't opened — blue dot. */
    unread?: boolean;
    /** SDR-020: the row's owning workspace, when the row renders outside its
     *  workspace group (the global pinned section) — surfaced as the row's
     *  hover hint, since the cross-workspace section has no header to say
     *  where a row belongs. Omitted inside a group (the header right above
     *  already says it). */
    workspaceName?: string;
  }>(),
  { approvalCount: 0, questionCount: 0, unread: false, workspaceName: '' },
);

// SDR-002: per-row work-facts (badges/spinner/outcome), read from the
// shared lookup the app provides — NOT from the sort projection, so the
// two derivations churn independently.
const resolveWorkFacts = inject<(sessionId: string) => SessionWorkFacts>(
  'resolveSessionWorkFacts',
  () => ({ busy: false }),
);
// The shared 30s relative-time tick — one timer for the whole sidebar;
// each row's time computed voids this ref (no per-row timers).
const sessionTimeClockTick = inject<() => number>('sessionTimeClockTick', () => 0);

// Interrupted-task recovery (Epic D): the in-place resume/dismiss actions —
// injected from App.vue (the resolveSessionWorkFacts precedent, finding 10),
// avoiding a WorkspaceGroup/Sidebar event re-emit chain. The defaults no-op
// so a row rendered outside App.vue (none today) stays inert.
const resumeInterruptedSession = inject<(id: string) => void>(
  'resumeInterruptedSession',
  () => {},
);
const dismissInterruptedSession = inject<(id: string) => void>(
  'dismissInterruptedSession',
  () => {},
);

const workFacts = computed<SessionWorkFacts>(() => resolveWorkFacts(props.session.id));
const busy = computed(() => workFacts.value.busy);
const pendingInteraction = computed(() => workFacts.value.pendingInteraction);
const lastTurnReason = computed(() => workFacts.value.lastTurnReason);
// ADR-010: the interrupted tag — the recovery list wins over the wire's
// per-session outcome, so this row reads interrupted while it is on the
// list, even over a settled outcome value.
const interrupted = computed(() =>
  interruptedBadgeVisible({
    renaming: renaming.value,
    busy: busy.value,
    lastTurnReason: lastTurnReason.value,
  }),
);
// Relative time follows the same recency key the sort uses (SDR-014),
// refreshed by the shared clock tick.
const time = computed(() => {
  void sessionTimeClockTick();
  return formatTime(props.session.lastUsedAt ?? props.session.updatedAt);
});

// SDR-020: outside a workspace group (the global pinned section) the row is the
// only thing saying where it belongs, so it carries its workspace as a hover
// hint. It hangs off the TITLE, not the time element: the row's hover design
// swaps the time for the action icons (visibility), so the time is exactly the
// element that disappears whenever a pointer is over the row — a hint there
// would never show. The title is the row's stable, always-visible target.
const workspaceHint = computed(() =>
  props.workspaceName ? t('sidebar.workspaceOfSession', { name: props.workspaceName }) : null,
);

const emit = defineEmits<{
  select: [id: string];
  rename: [id: string, title: string];
  archive: [id: string];
  /** SDR-016/017: pin with an optional color; unpin clears the trio. */
  pin: [id: string, color?: 'red' | 'orange' | 'yellow' | 'green' | 'blue' | 'purple' | 'gray'];
  unpin: [id: string];
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

// 重命名输入框：IME 组合期间 Enter 只上屏候选，Esc 不变。
function onRenameKeydown(e: KeyboardEvent): void {
  if (isCompositionKeyEvent(e)) return;
  if (e.key === 'Enter') {
    e.preventDefault();
    e.stopPropagation();
    commitRename();
  } else if (e.key === 'Escape') {
    e.preventDefault();
    e.stopPropagation();
    cancelRename();
  }
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
  if (e.key === 'Escape') {
    if (paletteOpen.value) {
      closePalette();
      return;
    }
    if (arming.value) disarm();
  }
}
// SDR-016/017: the seven-color preset (macOS folder labels).
const PIN_COLORS = ['red', 'orange', 'yellow', 'green', 'blue', 'purple', 'gray'] as const;
type PinColor = (typeof PIN_COLORS)[number];

// The color palette hangs off the pinned row's STATUS pin (next to the
// title), not the hover action. It renders position:fixed and is anchored
// to the trigger via getBoundingClientRect (the ws-menu pattern) so the
// scrolling session list can't clip it; it closes on outside mousedown,
// window resize, and scroll (the anchor moves with the row).
const paletteOpen = ref(false);
const paletteStyle = ref<Record<string, string>>({});
const statusPinRef = ref<HTMLButtonElement | null>(null);
const paletteEl = ref<HTMLElement | null>(null);

function closePalette(): void {
  paletteOpen.value = false;
  document.removeEventListener('mousedown', onPaletteDocClick);
  window.removeEventListener('resize', closePalette);
  window.removeEventListener('scroll', closePalette, true);
}

function onPaletteDocClick(e: MouseEvent): void {
  const target = e.target as Element;
  // Mousedown on the trigger or inside the palette belongs to the palette
  // flow (the trigger's click toggles it shut; swatch clicks run inside).
  if (target.closest('[data-test-id="session-pin-status"]')) return;
  if (target.closest('[data-test-id="session-pin-palette"]')) return;
  closePalette();
}

async function togglePalette(): Promise<void> {
  if (paletteOpen.value) {
    closePalette();
    return;
  }
  paletteOpen.value = true;
  document.addEventListener('mousedown', onPaletteDocClick);
  window.addEventListener('resize', closePalette);
  window.addEventListener('scroll', closePalette, true);
  await nextTick();
  const btn = statusPinRef.value;
  const palette = paletteEl.value;
  if (!btn) return;
  const r = btn.getBoundingClientRect();
  const gap = 4;
  const margin = 8;
  const menuH = palette?.offsetHeight ?? 0;
  const menuW = palette?.offsetWidth ?? 0;
  let top = r.bottom + gap;
  if (top + menuH > window.innerHeight - margin) {
    top = Math.max(margin, r.top - menuH - gap);
  }
  let left = r.left;
  if (left + menuW > window.innerWidth - margin) {
    left = Math.max(margin, window.innerWidth - margin - menuW);
  }
  if (left < margin) left = margin;
  paletteStyle.value = {
    top: `${Math.round(top)}px`,
    left: `${Math.round(left)}px`,
  };
}

// Pick a color = re-pin with it — no unpin first (the engine's set_pin
// keeps the pin moment and only follows the latest color).
function pickColor(color: PinColor): void {
  emit('pin', props.session.id, color);
  closePalette();
}

// "No color" = colorless re-pin: clears the color, keeps the session
// pinned (engine `session.pin` without a color writes a NULL color).
function clearColor(): void {
  emit('pin', props.session.id);
  closePalette();
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
  closePalette();
  if (typeof window !== 'undefined') window.removeEventListener('keydown', onKeydown);
});
if (typeof window !== 'undefined') {
  window.addEventListener('keydown', onKeydown);
}
</script>

<template>
  <div
    class="se"
    :class="{ on: active }"
    @click="emit('select', session.id)"
    @mouseleave="disarm"
  >
    <div class="row">
      <!-- Leading status slot (in the gutter left of the title): a spinner
           while the session runs, otherwise an unread blue dot. Fixed width
           so the title start never shifts. -->
      <span class="lead" aria-hidden="true">
        <Spinner v-if="busy" size="sm" />
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
          @keydown="onRenameKeydown"
          @blur="commitRename"
        />
        <!-- SDR-020: outside a workspace group (the global pinned section) the
             title carries the owning workspace as a hover hint — the section
             has no per-group header to say where a row belongs. Tooltip's
             wrapper is `display: contents`, so wrapping the title adds no box:
             the row's layout is unchanged whether or not the hint is passed. -->
        <Tooltip v-if="workspaceHint && !renaming" :text="workspaceHint">
          <span class="t" @dblclick.stop="startRename">{{ session.title }}</span>
        </Tooltip>
        <span v-else class="t" @dblclick.stop="startRename">{{ session.title }}</span>
        <!-- SDR-016/017: the pinned status pin — a persistent pushpin next
             to the title. It is the state marker AND the palette entry
             point (click opens the seven-color palette); the hover pin
             button in the action slot stays the quick pin/unpin toggle. -->
        <Tooltip v-if="session.pinned" :text="t('sidebar.pinned')">
          <button
            ref="statusPinRef"
            type="button"
            class="pin-status"
            :class="[`pin-${session.pinColor ?? 'gray'}`]"
            data-test-id="session-pin-status"
            :aria-label="t('sidebar.pinColorLabel')"
            @click.stop="togglePalette()"
          >
            <Icon name="pushpin" size="sm" />
          </button>
        </Tooltip>
      </div>

      <!-- Pending tags — coloured per kind, shown even when the row isn't
           active. "Answer" = an askUserQuestion is waiting; "Approve" = a
           permission request is waiting. The session's lifecycle status drives
           the same tags as a fallback for background sessions whose pending
           lists aren't loaded yet (status known, counts not). -->
      <Tooltip :text="t('workspace.awaitingAnswerTitle')">
        <Badge
          v-if="!renaming && (questionCount > 0 || pendingInteraction === 'question')"
          variant="info"
          size="sm"
        >
          {{ t('workspace.awaitingAnswer') }}
        </Badge>
      </Tooltip>
      <Tooltip :text="t('workspace.awaitingPermissionTitle')">
        <Badge
          v-if="!renaming && (approvalCount > 0 || pendingInteraction === 'approval')"
          variant="warning"
          size="sm"
        >
          {{ t('workspace.awaitingPermission') }}
        </Badge>
      </Tooltip>
      <!-- Aborted: a distinct, low-key error tag (not collapsed into idle). -->
      <Tooltip :text="t('workspace.abortedTitle')">
        <Badge
          v-if="!renaming && !busy && pendingInteraction !== 'question' && pendingInteraction !== 'approval' && questionCount === 0 && approvalCount === 0 && (lastTurnReason === 'cancelled' || lastTurnReason === 'failed')"
          variant="danger"
          size="sm"
        >
          {{ t('workspace.aborted') }}
        </Badge>
      </Tooltip>
      <!-- Stalled (ADR-004 O8-d): the watchdog's no-progress termination —
           its own word and a neutral variant, never rendered as a user stop
           or a failure. -->
      <Tooltip :text="t('workspace.stalledTitle')">
        <Badge
          v-if="!renaming && !busy && pendingInteraction !== 'question' && pendingInteraction !== 'approval' && questionCount === 0 && approvalCount === 0 && lastTurnReason === 'stalled'"
          variant="warning"
          size="sm"
        >
          {{ t('workspace.stalled') }}
        </Badge>
      </Tooltip>

      <!-- Interrupted (ADR-010, Epic D): the app exited while this session
           was running — its own word (zh「运行中断」), distinct from aborted's
           「已中断」, and an info variant so it never reads as a failure. -->
      <Tooltip :text="t('workspace.interruptedTitle')">
        <Badge
          v-if="interrupted && pendingInteraction !== 'question' && pendingInteraction !== 'approval' && questionCount === 0 && approvalCount === 0"
          variant="info"
          size="sm"
        >
          {{ t('workspace.interrupted') }}
        </Badge>
      </Tooltip>

      <!-- Compressed (session-retention): the wire log is gzip-compressed on
           disk — read/export stays transparent, only fork needs a restore. -->
      <Tooltip :text="t('workspace.compressedTitle')">
        <Badge
          v-if="!renaming && workFacts.compressed"
          variant="neutral"
          size="sm"
        >
          {{ t('workspace.compressed') }}
        </Badge>
      </Tooltip>

      <!-- Trailing action slot: the relative time and the hover icon buttons
           share one grid cell and swap via `visibility` (never display:none),
           so the slot width is identical in hover and rest. The badges and
           title therefore don't reflow on hover — see design-system §07
           "Session row". An interrupted row additionally carries inline
           resume/dismiss quick actions (Epic D: in-place recovery, the badge
           itself stays non-interactive) — they ride the same archive cell,
           so the grid stays single-column and the row never reflows between
           hover states. Full session actions (rename, fork, copy,
           open-in-finder) live in the ChatHeader `...` menu; this row
           exposes only fast shortcuts. Archive: click once to arm (danger
           state), click again to confirm — no modal. -->
      <span class="act">
        <span class="ts">{{ time }}</span>
        <Tooltip v-if="!renaming && interrupted" :text="t('recovery.resumeOne')">
          <IconButton
            class="rc"
            size="sm"
            :label="t('recovery.resumeOne')"
            @click.stop="resumeInterruptedSession(session.id)"
          >
            <Icon name="play" size="sm" />
          </IconButton>
        </Tooltip>
        <Tooltip v-if="!renaming && interrupted" :text="t('recovery.dismissOne')">
          <IconButton
            class="rc"
            size="sm"
            :label="t('recovery.dismissOne')"
            @click.stop="dismissInterruptedSession(session.id)"
          >
            <Icon name="close" size="sm" />
          </IconButton>
        </Tooltip>
        <!-- SDR-016/017: the quick pin action — an unpinned row pins
             colorless in one click; a pinned row unpins. Changing the
             color lives on the status pin's palette next to the title. -->
        <Tooltip v-if="!renaming" :text="session.pinned ? t('sidebar.unpin') : t('sidebar.pin')">
          <IconButton
            class="pin-btn"
            size="sm"
            :label="session.pinned ? t('sidebar.unpin') : t('sidebar.pin')"
            data-test-id="session-pin-btn"
            @click.stop="session.pinned ? emit('unpin', session.id) : emit('pin', session.id)"
          >
            <Icon name="pushpin" size="sm" />
          </IconButton>
        </Tooltip>
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

    <!-- SDR-016/017: the pinned side bar — a 3px rounded strip on the row's
         left edge carrying the pin color (neutral gray when colorless).
         Decorative state, so aria-hidden; absolutely positioned inside the
         row's padding so it never shifts the title start. -->
    <span
      v-if="session.pinned"
      class="pin-bar"
      :class="[`pin-${session.pinColor ?? 'gray'}`]"
      data-test-id="session-pin-bar"
      aria-hidden="true"
    />

    <!-- The seven-color palette (plus "no color"), anchored to the status
         pin via position:fixed so the scrolling list can't clip it. -->
    <div
      v-if="paletteOpen"
      ref="paletteEl"
      class="pin-palette"
      data-test-id="session-pin-palette"
      :style="paletteStyle"
      role="menu"
      @click.stop
    >
      <button
        v-for="color in PIN_COLORS"
        :key="color"
        type="button"
        role="menuitemradio"
        class="pin-sw"
        :class="[`pin-${color}`, { on: session.pinColor === color }]"
        :aria-checked="session.pinColor === color"
        :aria-label="t('sidebar.pinColor', { color })"
        @click.stop="pickColor(color)"
      />
      <button
        type="button"
        role="menuitemradio"
        class="pin-sw pin-clear"
        :class="{ on: !session.pinColor }"
        :aria-checked="!session.pinColor"
        :aria-label="t('sidebar.pinNoColor')"
        data-test-id="session-pin-clear"
        @click.stop="clearColor()"
      >
        {{ t('sidebar.pinNoColor') }}
      </button>
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

/* Trailing action slot: time and the hover icon buttons share one grid cell
   (grid-area:1/1). All stay in the layout and swap via `visibility` (never
   display:none), so within one row state the slot width is identical in
   hover and rest — the badges and title don't reflow, eliminating hover
   jitter. `.act .arc` / `.act .rc` out-specificity IconButton's own display
   so the hidden default wins. The interrupted quick actions (`.rc`) render
   whenever the row is interrupted (both at hover and at rest), so only the
   rare recovery-list change — not hovering — can resize the slot. */
.act {
  display: inline-grid;
  grid-auto-flow: column;
  flex: none;
  align-items: center;
  justify-items: center;
}
.act .ts,
.act .arc { grid-area: 1 / 1; }
.act .arc,
.act .rc { visibility: hidden; }
.se:hover .act .arc,
.act .arc.armed,
.se:hover .act .rc { visibility: visible; }
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

/* SDR-016/017: pin visuals. The seven preset colors are global tokens
   (--pin-* in style.css) surfaced through the `pin-<color>` classes as
   `--pin-color`, so the status pin, the side bar, and the palette
   swatches all read one variable per color. */

/* The pinned status pin — a persistent pushpin next to the title; its
   icon carries the pin color and clicking it opens the palette. */
.sessions .se .pin-status {
  flex: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  margin-left: 6px;
  padding: 0;
  border: none;
  background: none;
  cursor: pointer;
  color: var(--pin-color, var(--pin-gray));
}
.sessions .se .pin-status:hover { filter: brightness(1.15); }
.sessions .se .pin-btn { border-radius: var(--radius-sm); }

/* The pinned side bar — 3px × 14px rounded strip on the row's left edge,
   inside the row padding so the title start never shifts. */
.sessions .se .pin-bar {
  position: absolute;
  left: 2px;
  top: 50%;
  transform: translateY(-50%);
  width: 3px;
  height: 14px;
  border-radius: 2px;
  background: var(--pin-color, var(--pin-gray));
  box-shadow: inset 0 0 0 1px rgba(0, 0, 0, 0.2);
}

/* The seven-color palette popover — position:fixed (anchored to the
   status pin in script) so the scrolling session list can't clip it. */
.sessions .se .pin-palette {
  position: fixed;
  z-index: var(--z-dropdown);
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 6px;
  border-radius: var(--radius-md);
  background: var(--bg-2, var(--color-bg-2, #2a2a30));
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.35);
}
.sessions .se .pin-sw {
  width: 16px;
  height: 16px;
  border-radius: 50%;
  border: none;
  cursor: pointer;
  padding: 0;
  background: var(--pin-color, var(--pin-gray));
}
/* The current choice gets a ring so the active color reads at a glance. */
.sessions .se .pin-sw.on {
  box-shadow:
    0 0 0 2px var(--bg-2, var(--color-bg-2, #2a2a30)),
    0 0 0 4px var(--color-text);
}
/* "No color" — a text chip, not a color dot; neutral surface, same ring
   treatment when it is the current choice. */
.sessions .se .pin-sw.pin-clear {
  width: auto;
  height: 22px;
  padding: 0 8px;
  border-radius: var(--radius-full);
  background: var(--color-surface-sunken);
  color: var(--color-text-muted);
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  line-height: 22px;
}
.sessions .se .pin-sw.pin-clear.on {
  color: var(--color-text);
  box-shadow:
    0 0 0 2px var(--bg-2, var(--color-bg-2, #2a2a30)),
    0 0 0 4px var(--color-text);
}
</style>
