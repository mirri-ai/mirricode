<!-- apps/mirri-web/src/components/SessionLeadIndicator.vue -->
<!-- The session row's leading slot as ONE indicator — the row's whole state in
     the gutter the sidebar already had, with no extra column.

     Six states, decided from three booleans (pinned / busy / unread):

       ① unpinned · idle    empty at rest; a hollow "pin here" ring on row hover
       ② unpinned · unread  one neutral dot (the accent colour)
       ③ unpinned · running hollow ring + a sweeping arc (the loader's meaning)
       ④ pinned · idle      a filled circle in the pin colour
       ⑤ pinned · unread    ④ plus a corner badge, punched out by 2px of the row's
                            own background so it never has to contrast with the
                            seven pin colours
       ⑥ pinned · running   ④ plus a breathing glow (McpServersPanel's glow-pulse)

     `busy` wins over `unread`: a running row says one thing, not two.

     The component owns the visual state AND the pointer/keyboard intent (the
     hover dwell that opens the colour palette). It emits `toggle` and
     `openPalette` — the palette popover belongs to the row, which does the
     fixed positioning. State ① is revealed by the ROW on hover/focus (a
     reusable indicator must not know what a row is), so the row styles
     `.orb.is-idle-unpinned` through `:deep`. -->
<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from 'vue';

import { DEFAULT_PIN_COLOR, type PinColor } from '../lib/pinColor';
import Tooltip from './ui/Tooltip.vue';

/** Hover dwell before the palette opens. Long enough that a pointer sweeping
 *  across the row never triggers it — only a deliberate stop does. */
const PIN_HOVER_OPEN_MS = 600;

const props = withDefaults(
  defineProps<{
    /** The session is pinned. */
    pinned?: boolean;
    /** The pin's colour; a legacy pin without one falls back to the default. */
    pinColor?: PinColor | null;
    /** The session is running a turn. */
    busy?: boolean;
    /** A background turn finished and the user has not opened the row yet. */
    unread?: boolean;
    /** Accessible name — the row builds it from the session title. */
    label: string;
    /** Hover hint for the unpinned affordance; absent on a pinned row. */
    hint?: string | null;    /** The colour palette is open — stops the hover dwell from re-firing. */
    paletteOpen?: boolean;
    /** Renaming: keep the indicator visible but inert. */
    interactive?: boolean;
  }>(),
  {
    pinned: false,
    pinColor: null,
    busy: false,
    unread: false,
    paletteOpen: false,
    interactive: true,
  },
);

const emit = defineEmits<{
  /** Plain activation: pin with the default colour, or unpin. */
  toggle: [];
  /** The pointer settled on a pinned dot, or the keyboard asked for the
   *  palette. Carries the anchor element the popover hangs off. */
  openPalette: [anchor: HTMLElement | null];
  /** The pointer left the dot — the row decides whether to close the palette. */
  leave: [];
}>();

const el = ref<HTMLButtonElement | null>(null);

const color = computed<PinColor>(() => props.pinColor ?? DEFAULT_PIN_COLOR);
/** Unread lives in the corner badge, and only when the row is not running. */
const showUnread = computed(() => props.unread && !props.busy);
/** State ①: nothing to draw until the row is hovered. */
const isIdleUnpinned = computed(() => !props.pinned && !props.busy && !props.unread);

const classes = computed(() => ({
  'is-pinned': props.pinned,
  'is-running': props.busy,
  'is-unread': showUnread.value,
  'is-idle-unpinned': isIdleUnpinned.value,
  [`pin-${color.value}`]: props.pinned,
}));

let hoverOpenTimer: ReturnType<typeof setTimeout> | null = null;
function cancelHoverOpen(): void {
  if (hoverOpenTimer !== null) {
    clearTimeout(hoverOpenTimer);
    hoverOpenTimer = null;
  }
}
function schedulePalette(): void {
  cancelHoverOpen();
  hoverOpenTimer = setTimeout(() => {
    hoverOpenTimer = null;
    emit('openPalette', el.value);
  }, PIN_HOVER_OPEN_MS);
}
/** Only a pinned dot has a palette to open. */
function onEnter(): void {
  if (!props.pinned || props.paletteOpen) return;
  schedulePalette();
}
/** Moving restarts the dwell: a pointer passing through never opens it. */
function onMove(): void {
  if (!props.pinned || props.paletteOpen) return;
  schedulePalette();
}
function onLeave(): void {
  cancelHoverOpen();
  emit('leave');
}
/** Shift+F10 / the ContextMenu key — the keyboard equivalents of a right-click.
 *  Enter/Space keep their native button behaviour (pin/unpin). */
function onKeydown(e: KeyboardEvent): void {
  if (e.key === 'ContextMenu' || (e.key === 'F10' && e.shiftKey)) {
    e.preventDefault();
    if (props.pinned) emit('openPalette', el.value);
  }
}

onBeforeUnmount(cancelHoverOpen);
</script>

<template>
  <!-- `Tooltip`'s wrapper is `display: contents`, so it adds no box: the row's
       layout is identical whether or not a hint is passed. A pinned dot gets no
       hint — hovering it opens the palette, and a bubble would fight the popover
       (its aria-label already names the session for assistive tech). -->
  <Tooltip :text="hint">
    <button
      v-if="interactive"
      ref="el"
      type="button"
      class="orb"
      :class="classes"
      data-test-id="session-pin-btn"
      :aria-pressed="pinned"
      :aria-busy="busy || undefined"
      :aria-label="label"
      @click.stop="emit('toggle')"
      @mouseenter="onEnter"
      @mousemove="onMove"
      @mouseleave="onLeave"
      @contextmenu.prevent.stop="pinned && emit('openPalette', el)"
      @keydown="onKeydown"
    >
      <!-- Running: a proper spinner — a rounded arc over a faint track, both in
           the same ring colour. This replaces an earlier "ring with a sector cut
           out" (a conic gradient); at 13px a hard-edged notch reads as a chip
           missing from the circle rather than motion, and its edges alias badly.
           A round-capped SVG stroke is crisp at any size and the arc's ends read
           as intentional. -->
      <svg
        v-if="busy"
        class="orb-ring"
        viewBox="0 0 13 13"
        aria-hidden="true"
        focusable="false"
      >
        <circle class="orb-ring-track" cx="6.5" cy="6.5" r="5" />
        <circle class="orb-ring-arc" cx="6.5" cy="6.5" r="5" />
      </svg>
      <span v-else class="orb-dot" aria-hidden="true" />
      <span v-if="showUnread" class="orb-unread" aria-hidden="true" />
    </button>
    <span v-else class="orb" :class="classes" aria-hidden="true">
      <svg v-if="busy" class="orb-ring" viewBox="0 0 13 13" focusable="false">
        <circle class="orb-ring-track" cx="6.5" cy="6.5" r="5" />
        <circle class="orb-ring-arc" cx="6.5" cy="6.5" r="5" />
      </svg>
      <span v-else class="orb-dot" />
      <span v-if="showUnread" class="orb-unread" />
    </span>
  </Tooltip>
</template>

<style scoped>
/* The hit target is 20×20 — the lead slot's own width — so the circle is easy
   to hit without the row's left gutter becoming a minefield. */
.orb {
  position: relative;
  width: 20px;
  height: 20px;
  flex: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: 0;
  border: none;
  border-radius: var(--radius-full);
  background: none;
  cursor: pointer;
  /* The ring colour, used only while running: the pin colour when pinned, a
     neutral one when not. ONE property drives both running states, so the
     sidebar has exactly one "working" language. */
  --ring-color: color-mix(in srgb, var(--color-text-muted) 70%, transparent);
}
.orb.is-pinned {
  --ring-color: var(--pin-color, var(--pin-red));
}
.orb:focus-visible {
  outline: none;
  box-shadow: var(--p-focus-ring);
}

/* The marker: ONE 13px circle in every state. Hollow vs filled carries the
   meaning, so the shape never has to be learned twice and colour is never the
   only signal. */
.orb-dot {
  display: block;
  width: 13px;
  height: 13px;
  border-radius: var(--radius-full);
  transition:
    background var(--duration-fast) var(--ease-out),
    box-shadow var(--duration-fast) var(--ease-out);
}

/* ① unpinned · idle — nothing at rest. The row reveals this on hover/focus,
   and until then it must not swallow clicks meant for the row itself. */
.orb.is-idle-unpinned {
  opacity: 0;
  pointer-events: none;
  transition: opacity var(--duration-fast) var(--ease-out);
}
.orb.is-idle-unpinned .orb-dot {
  box-shadow: inset 0 0 0 1.5px var(--color-text-faint);
}

/* ② unpinned · unread — a neutral dot. `--color-accent` is near-black on the
   light theme and near-white on the dark one, so it reads on both without a
   second colour vocabulary. */
.orb.is-unread:not(.is-pinned) .orb-dot {
  background: var(--color-accent);
}

/* ③ + ⑥ running — ONE language for both states: the dot becomes a spinner,
   a rounded arc travelling around a faint track. Only the ring's colour differs
   (see --ring-color), so a user never has to learn two different "working"
   readings.

   This replaced two earlier attempts at the same slot: an outer glow (nearly
   invisible at 13px, it dimmed the pin colour, and glow is already this
   product's "connected" idiom on the MCP panel) and a conic ring with a sector
   cut out of it (a hard notch reads as a chip MISSING from the circle rather
   than as motion, and its edges alias badly at this size). A round-capped stroke
   is crisp at any size and its two ends read as deliberate. */
.orb-ring {
  width: 13px;
  height: 13px;
  flex: none;
  transform-origin: 50% 50%;
  animation: orb-ring-spin 0.9s linear infinite;
}
.orb-ring-track,
.orb-ring-arc {
  fill: none;
  stroke: var(--ring-color);
  stroke-width: 1.8;
}
/* A faint full ring: it gives the arc a context to travel around, and keeps the
   "there is a spinner here" read even at the arc's most contracted frame. */
.orb-ring-track {
  opacity: 0.22;
}
/* About a third of the circumference, with rounded ends. */
.orb-ring-arc {
  stroke-linecap: round;
  stroke-dasharray: 10.5 21;
}
@keyframes orb-ring-spin {
  to {
    transform: rotate(360deg);
  }
}
/* ④ pinned · idle — filled with the pin colour. The hairline inset ring keeps
   the dot legible on light and dark surfaces. */
.orb.is-pinned .orb-dot {
  background: var(--pin-color, var(--pin-red));
  box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--pin-color, var(--pin-red)) 60%, #000);
}

/* ⑤ unread badge. The 2px ring of the row's own background punches the badge
   out of the circle, so it only ever contrasts with the ROW, never with the
   seven pin colours — no per-colour contrast maths. */
.orb-unread {
  position: absolute;
  top: 1px;
  right: 1px;
  width: 6px;
  height: 6px;
  border-radius: var(--radius-full);
  background: var(--color-accent);
  box-shadow: 0 0 0 2px var(--row-bg, var(--panel));
}

@media (prefers-reduced-motion: reduce) {
  /* A spinner still reads as "working" while stationary: the arc stays parked
     with its rounded ends and the faint track keeps the ring shape, which is
     distinct from both the solid pinned dot and the hollow idle ring. Slowing
     it (rather than freezing it dead) also avoids the arc looking like a
     rendering artefact. */
  .orb-ring {
    animation-duration: 2.4s;
  }
}
</style>
