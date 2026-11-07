<!-- apps/mirri-web/src/components/ui/Tooltip.vue -->
<!-- Design-system §03 Tooltip: hover/focus hint. Wrap the trigger in the default
     slot; text via prop. The wrapper is `display: contents` so it never alters the
     trigger's layout (safe for truncated/flex triggers); listeners are attached to
     the real trigger element, which also anchors the bubble, and re-attached if that
     element is removed or replaced (so an open tooltip can never strand on screen).
     The bubble is rendered through a body teleport so it escapes ancestor overflow
     clipping, and positioned with flip + viewport clamping. Short text stays on one
     line; long text wraps within `maxWidth` and is clamped to `maxLines` lines with
     an ellipsis so the bubble never grows too tall.
     `variant="code"` (payload bubbles): monospace, pre-formatted, left-aligned with
     the trigger, and it grows UPWARD out of the anchor's top edge so a row near the
     bottom of a long conversation never pushes the bubble underneath it. It is capped
     at `codeLines` lines; a longer payload scrolls inside the bubble. -->
<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue';

type Placement = 'top' | 'bottom' | 'left' | 'right';
type Variant = 'text' | 'code';

const props = withDefaults(
  defineProps<{
    text?: string | null;
    placement?: Placement;
    maxWidth?: number;
    /** Clamp the bubble to at most this many lines (with an ellipsis). */
    maxLines?: number;
    /** `code` renders a monospace payload bubble (see the file header). */
    variant?: Variant;
    /** `code` only: bubble height cap, in lines. A longer payload scrolls. */
    codeLines?: number;
    /** Size the bubble to the transcript's reading column instead of a fixed
     *  px width — a payload bubble should read like the row's expanded detail,
     *  not a floating chip. The width comes from the anchor's inherited
     *  `--read-max` (the reading-column width ConversationPane sets); when there
     *  is no reading column (e.g. the side-chat panel) it falls back to the
     *  nearest scroll container's width, then to `maxWidth`. Always clamped to
     *  the viewport. */
    column?: boolean;
  }>(),
  {
    placement: 'top',
    maxWidth: 280,
    maxLines: 6,
    variant: 'text',
    codeLines: 4,
    column: false,
  },
);

const GAP = 6;
const MARGIN = 8;
const LINE_HEIGHT = 1.5;
const CODE_VPAD = 12; // vertical padding of a payload bubble (.ui-tip__bubble.code)
const SHOW_DELAY = 150;
/** Grace period before a payload bubble closes, so the pointer can travel from
 *  the trigger into the bubble (and back) to scroll a long command. */
const HIDE_GRACE_MS = 180;

const isCode = computed(() => props.variant === 'code');

const trigger = ref<HTMLElement>();
const bubble = ref<HTMLElement>();
const open = ref(false);
const positioned = ref(false);
/** True when the payload bubble is showing a capped slice — drives the bottom
 *  fade that tells the reader there is more. */
const clipped = ref(false);
const bubbleStyle = ref<Record<string, string>>({ maxWidth: `${props.maxWidth}px` });

let showTimer: ReturnType<typeof setTimeout> | undefined;
let hideTimer: ReturnType<typeof setTimeout> | undefined;
/** Set by `columnWidth` for column-mode bubbles: the reading column's left edge,
 *  which the bubble aligns to. */
let columnLeft: number | undefined;
let target: HTMLElement | null = null;
let observer: MutationObserver | undefined;

/** Refresh the "there is more below" fade. The mask is applied only while the
 *  bubble is actually scrolled away from its bottom — a permanent fade over a
 *  fully-scrolled bubble would dim real content and imply content that isn't
 *  there. */
function refreshClipped(): void {
  const el = bubble.value;
  clipped.value =
    isCode.value && !!el && el.scrollHeight > el.clientHeight + 1 &&
    el.scrollTop + el.clientHeight < el.scrollHeight - 1;
}

function onBubbleScroll(): void {
  refreshClipped();
}

function position(): void {
  const bub = bubble.value;
  if (!target || !bub) return;
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const maxW = columnWidth(vw);
  // Apply the width BEFORE measuring: a wide bubble must be measured at the
  // width it will actually render at, or the clamp/centring maths runs on the
  // previous show's geometry.
  bub.style.maxWidth = `${maxW}px`;
  const r = target.getBoundingClientRect();
  const sz = bub.getBoundingClientRect();
  const bw = sz.width;
  // A payload bubble's height is its line cap, rebuilt from the stylesheet so no
  // measuring pass can go stale.
  const bh = isCode.value ? props.codeLines * lineHeight() + CODE_VPAD : sz.height;

  let place = props.placement;
  // Payload bubbles always open upward and grow to `codeLines` above the anchor, so
  // a row near the bottom of the conversation never covers itself; a payload taller
  // than that scrolls inside the bubble instead of being cut.
  if (isCode.value) place = 'top';
  else if (place === 'top' && r.top - GAP - bh < MARGIN) place = 'bottom';
  else if (place === 'bottom' && r.bottom + GAP + bh > vh - MARGIN) place = 'top';

  if (place === 'left' && r.left - GAP - bw < MARGIN) place = 'right';
  else if (place === 'right' && r.right + GAP + bw > vw - MARGIN) place = 'left';

  let top = 0;
  let left = 0;
  if (place === 'top') {
    top = r.top - GAP - bh;
    left = r.left + r.width / 2 - bw / 2;
  } else if (place === 'bottom') {
    top = r.bottom + GAP;
    left = r.left + r.width / 2 - bw / 2;
  } else if (place === 'left') {
    top = r.top + r.height / 2 - bh / 2;
    left = r.left - GAP - bw;
  } else {
    top = r.top + r.height / 2 - bh / 2;
    left = r.right + GAP;
  }

  // Payload bubbles align with the row's left edge, not its centre. In column
  // mode they align with the reading column's left edge, so the bubble lines up
  // with the conversation area instead of starting at the nested card.
  if (isCode.value) left = columnLeft ?? r.left;

  left = Math.min(Math.max(left, MARGIN), Math.max(vw - MARGIN - bw, MARGIN));
  top = Math.min(Math.max(top, MARGIN), Math.max(vh - MARGIN - bh, MARGIN));

  bubbleStyle.value = {
    maxWidth: `${maxW}px`,
    top: `${Math.round(top)}px`,
    left: `${Math.round(left)}px`,
    ...(isCode.value ? { maxHeight: `${bh}px` } : undefined),
  };
  // The style above is applied on the next tick; only then can `scrollHeight`
  // report the real overflow, which is what decides whether the bottom fade
  // shows. Measuring synchronously here would read the previous show's box.
  void nextTick(refreshClipped);
}

/** The bubble's max width. Fixed `maxWidth` unless `column` is set, in which
    case it follows the transcript's reading column so the bubble reads as the
    row's own detail rather than a floating chip. Always clamped to the viewport
    (and re-measured on every show, so a resized window cannot leave it too
    wide). */
function columnWidth(vw: number): number {
  const cap = Math.max(vw - MARGIN * 2, 200);
  if (!props.column) return Math.min(props.maxWidth, cap);
  const el = target ?? trigger.value;
  if (!el) return Math.min(props.maxWidth, cap);
  const column = readColumnRect(el);
  if (column) {
    columnLeft = column.left;
    return Math.min(column.width, cap);
  }
  // No reading column (the side-chat panel): the transcript spans its scroll
  // container, so that container's width is the honest answer.
  columnLeft = undefined;
  return Math.min(scrollContainerWidth(el) ?? props.maxWidth, cap);
}

/** Width of the nearest scrolling ancestor — the transcript pane whose width the
    reading column is bounded by. */
function scrollContainerWidth(el: HTMLElement): number | undefined {
  for (let p = el.parentElement; p; p = p.parentElement) {
    if (/(auto|scroll)/.test(getComputedStyle(p).overflowY)) {
      const w = p.getBoundingClientRect().width;
      if (w > 0) return w;
    }
  }
  return undefined;
}

/** The reading column's box, when the transcript declares one. The column is the
    outermost ancestor whose rendered width is bounded by `--read-max` (the
    `.content-wrap` around the turn blocks) — everything above it is the wider
    scrolling pane, everything below is the tool card inside the column. */
function readColumnRect(el: HTMLElement): { left: number; width: number } | undefined {
  let readMax: number | undefined;
  for (let p: HTMLElement | null = el; p; p = p.parentElement) {
    const raw = getComputedStyle(p).getPropertyValue('--read-max').trim();
    const px = parseFloat(raw);
    if (raw.endsWith('px') && Number.isFinite(px) && px > 0) {
      readMax = px;
      break;
    }
  }
  if (readMax === undefined) return undefined;
  let column: DOMRect | undefined;
  for (let p = el.parentElement; p; p = p.parentElement) {
    const r = p.getBoundingClientRect();
    // Stop at the first ancestor wider than the column: that is the pane.
    if (r.width > readMax + 1) break;
    column = r;
  }
  return column ? { left: column.left, width: column.width } : undefined;
}

/** One line box of the payload bubble (--text-xs at the 1.5 line-height in the CSS). */
function lineHeight(): number {
  const root = parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
  return root * 0.75 * LINE_HEIGHT;
}

function show(): void {
  if (!props.text) return;
  window.clearTimeout(showTimer);
  window.clearTimeout(hideTimer);
  showTimer = window.setTimeout(() => {
    open.value = true;
    positioned.value = false;
    void nextTick(() => {
      position();
      positioned.value = true;
    });
  }, SHOW_DELAY);
}

/** Dismiss the bubble now. */
function hideNow(): void {
  window.clearTimeout(showTimer);
  window.clearTimeout(hideTimer);
  open.value = false;
  positioned.value = false;
}

function hide(): void {
  window.clearTimeout(showTimer);
  // A payload bubble is reachable (it takes pointer events so it can be
  // scrolled) and sits HIDE_GRACE_MS's worth of gap away from its trigger, so
  // leaving the trigger must not close it before the pointer arrives. Plain
  // text hints stay unreachable and close immediately.
  if (!isCode.value) {
    hideNow();
    return;
  }
  window.clearTimeout(hideTimer);
  hideTimer = window.setTimeout(hideNow, HIDE_GRACE_MS);
}

/** Keep the bubble alive while the pointer is over it: a payload bubble can be
 *  taller than its line cap, and the overflow is meant to be scrolled. */
function onBubbleEnter(): void {
  window.clearTimeout(showTimer);
  window.clearTimeout(hideTimer);
}

/** Leaving the bubble closes it after the same grace, so the pointer can cross
 *  back to the trigger without the bubble flickering. */
function onBubbleLeave(): void {
  window.clearTimeout(hideTimer);
  hideTimer = window.setTimeout(hideNow, HIDE_GRACE_MS);
}

function onScrollOrResize(e?: Event): void {
  // Scrolling INSIDE the payload bubble is the user reading a long command —
  // it must not dismiss the bubble it is scrolling.
  if (e?.target instanceof Node && bubble.value?.contains(e.target)) return;
  if (open.value) hide();
}

function setTarget(el: HTMLElement | null): void {
  if (el === target) return;
  if (target) {
    target.removeEventListener('mouseenter', show);
    target.removeEventListener('mouseleave', hide);
    target.removeEventListener('focusin', show);
    target.removeEventListener('focusout', hide);
  }
  target = el;
  if (target) {
    target.addEventListener('mouseenter', show);
    target.addEventListener('mouseleave', hide);
    target.addEventListener('focusin', show);
    target.addEventListener('focusout', hide);
  }
}

onMounted(() => {
  const root = trigger.value ?? null;
  setTarget((root?.firstElementChild as HTMLElement | null) ?? root);
  // Keep `target` in sync with the live slotted element: if it's removed or
  // replaced while the tooltip is open (e.g. a v-if toggles on hover), the
  // mouseleave we rely on never fires and the bubble would get stuck on screen.
  if (root) {
    observer = new MutationObserver(() => {
      const next = (root.firstElementChild as HTMLElement | null) ?? null;
      if (next !== target) {
        hide();
        setTarget(next ?? root);
      }
    });
    observer.observe(root, { childList: true });
  }
  window.addEventListener('scroll', onScrollOrResize, true);
  window.addEventListener('resize', onScrollOrResize);
});

onBeforeUnmount(() => {
  window.clearTimeout(showTimer);
  window.clearTimeout(hideTimer);
  observer?.disconnect();
  setTarget(null);
  window.removeEventListener('scroll', onScrollOrResize, true);
  window.removeEventListener('resize', onScrollOrResize);
});
</script>

<template>
  <span ref="trigger" class="ui-tip">
    <slot />
  </span>
  <Teleport to="body">
    <div
      ref="bubble"
      v-show="open"
      class="ui-tip__bubble"
      :class="{ positioned, code: isCode, clipped: clipped && isCode }"
      :style="[bubbleStyle, isCode ? undefined : { '--tip-lines': maxLines }]"
      role="tooltip"
      @mouseenter="onBubbleEnter"
      @mouseleave="onBubbleLeave"
      @scroll.passive="onBubbleScroll"
    >
      <span v-if="isCode" class="ui-tip__code">{{ text }}</span>
      <template v-else>{{ text }}</template>
    </div>
  </Teleport>
</template>

<style scoped>
.ui-tip { display: contents; }
.ui-tip__bubble {
  position: fixed;
  z-index: var(--z-tooltip);
  display: -webkit-box;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: var(--tip-lines);
  max-width: 280px;
  padding: 4px 8px;
  border-radius: var(--radius-sm);
  background: var(--color-text);
  color: var(--color-bg);
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  line-height: 1.35;
  overflow: hidden;
  overflow-wrap: anywhere;
  /* Plain text hints never take the pointer (a hover hint must not swallow
     clicks); the payload bubble overrides this to `auto` so it can be scrolled. */
  pointer-events: none;
  opacity: 0;
  transition: opacity var(--duration-fast) var(--ease-out);
}
.ui-tip__bubble.positioned { opacity: 1; }

/* Payload bubble: the pre-formatted argument (pretty JSON), capped at `codeLines`
   lines; the overflow scrolls inside the bubble so every field stays reachable.
   Unlike the plain text hint it DOES take pointer events (`pointer-events: auto`
   below) — a bubble you cannot scroll is a bubble that hides part of the
   command. It opens upward with a 6px gap, so it never sits over its trigger
   and hovering into it cannot re-trigger the show path. */
.ui-tip__bubble.code {
  display: block;
  padding: 6px 9px;
  font-family: var(--font-mono);
  line-height: 1.5;
  /* Vertical scroll only: the payload wraps (`.ui-tip__code`), so a horizontal
     scrollbar would just cost height without revealing anything. */
  overflow-y: auto;
  overflow-x: hidden;
  overscroll-behavior: contain;
  pointer-events: auto;
}
/* The bubble is capped at `codeLines`; when the command is longer, the capping
   must be VISIBLE or a truncated command reads as the complete one. macOS
   overlay scrollbars fade out until you scroll, so a scrollbar alone is not a
   reliable signal — this mask fades the bottom edge instead. `.clipped` is set
   only while the content actually overflows. */
.ui-tip__bubble.code.clipped {
  mask-image: linear-gradient(to bottom, #000 calc(100% - 10px), transparent 100%);
}
.ui-tip__bubble.code::-webkit-scrollbar {
  width: 8px;
  height: 0;
}
.ui-tip__bubble.code::-webkit-scrollbar-track {
  background: transparent;
}
.ui-tip__bubble.code::-webkit-scrollbar-thumb {
  border-radius: var(--radius-full);
  background: color-mix(in srgb, var(--color-bg) 45%, transparent);
}
.ui-tip__code {
  display: block;
  width: max-content;
  max-width: 100%;
  /* Commands and JSON keep their own line breaks; over-long lines wrap instead
     of scrolling sideways (a horizontally scrolled hint is unreadable), and the
     bubble's line cap turns the remainder into vertical scroll. */
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
</style>
