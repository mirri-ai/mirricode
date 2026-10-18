// apps/mirri-web/src/lib/scrollFollow.ts
import { nextTick, ref, type Ref } from 'vue';

/** Within this many pixels from the bottom counts as "at the bottom" — being
 *  this close re-enables the follow; scrolling further up stops it. Mirrors
 *  ConversationPane's BOTTOM_THRESHOLD so the side chat transcript follows the
 *  same convention as the main conversation pane. */
export const BOTTOM_FOLLOW_THRESHOLD_PX = 80;

/** True when the scroll container's viewport is within the follow threshold
 *  of its content bottom (or the content does not overflow at all). Read the
 *  position BEFORE the DOM grows: once new content lands, it inflates the
 *  distance and a user who was "at the bottom" would stop following. */
export function isNearBottom(
  el: { scrollHeight: number; scrollTop: number; clientHeight: number },
  threshold: number = BOTTOM_FOLLOW_THRESHOLD_PX,
): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight < threshold;
}

/** Any scroll delta below this magnitude (px) is treated as noise (sub-pixel
 *  rounding, momentum tail) rather than a directional user input. */
const SCROLL_DIRECTION_EPSILON_PX = 1;

export interface ScrollFollow {
  /** True while the panel keeps its viewport pinned to the bottom. */
  following: Ref<boolean>;
  /** Call on the container's `scroll` event — maintains the follow state from
   *  the observed scroll direction (up ⇒ unfollow, down into the bottom zone
   *  ⇒ re-follow). */
  onScroll(): void;
  /** Call when streaming content landed in the DOM — if still following, the
   *  bottom pin is re-applied after the layout settles (nextTick). Returns a
   *  promise for testability; fire-and-forget at the call site. */
  onContentGrow(): Promise<void>;
  /** Call on raw wheel/touch input (which arrives BEFORE the `scroll` event)
   *  with the input's direction. Upward intent stops the follow immediately
   *  and cancels any queued pin, so a queued write cannot fight the user. */
  onUserIntent(direction: 'up' | 'down'): void;
  /** Unconditionally pin to the bottom and re-enable the follow (e.g. the
   *  user sent a new message). */
  scrollToBottom(): void;
}

/** Direction-based bottom-follow state for a scrolling panel — the pattern
 *  ConversationPane uses. The naive "still within N px of the bottom ⇒ keep
 *  writing scrollTop = scrollHeight" check loses against streaming output:
 *  every pin write resets the distance to 0, so a trackpad flick up of a few
 *  px never accumulates past N before the next delta pins it back — the user
 *  can never scroll up. Here an observed upward direction (or upward wheel/
 *  touch input, which arrives before `scroll`) stops the follow outright; the
 *  follow resumes only when the user scrolls back down into the bottom zone.
 *  `container` is a getter (not a captured element) so it tolerates the
 *  ref-not-yet-mounted window. */
export function createScrollFollow(
  container: () => { scrollHeight: number; scrollTop: number; clientHeight: number } | null,
): ScrollFollow {
  const following = ref(true);
  let lastScrollTop = 0;
  // Monotonic token: bumped whenever the follow is cancelled so a queued
  // nextTick pin from an already-arrived delta cannot resurrect it.
  let pinGeneration = 0;

  function current(): { scrollHeight: number; scrollTop: number; clientHeight: number } | null {
    return container();
  }

  function canScroll(): boolean {
    const el = current();
    return !!el && el.scrollHeight - el.clientHeight > 1;
  }

  function onScroll(): void {
    const el = current();
    if (!el) return;
    const top = el.scrollTop;
    const dist = el.scrollHeight - top - el.clientHeight;
    if (top < lastScrollTop - SCROLL_DIRECTION_EPSILON_PX && dist > SCROLL_DIRECTION_EPSILON_PX) {
      following.value = false;
    } else if (
      top > lastScrollTop + SCROLL_DIRECTION_EPSILON_PX &&
      dist <= BOTTOM_FOLLOW_THRESHOLD_PX
    ) {
      following.value = true;
    }
    lastScrollTop = top;
  }

  async function onContentGrow(): Promise<void> {
    if (!following.value || !canScroll()) return;
    const generation = pinGeneration;
    await nextTick();
    // The user may have scrolled up (or shown upward intent) between the
    // content landing and the layout settling — don't yank them back down.
    if (!following.value || generation !== pinGeneration) return;
    const el = current();
    if (el) {
      el.scrollTop = el.scrollHeight;
      lastScrollTop = el.scrollTop;
    }
  }

  function onUserIntent(direction: 'up' | 'down'): void {
    if (direction === 'down') return; // downward input never breaks the pin
    if (!canScroll()) return; // nothing to scroll up to — keep following
    following.value = false;
    pinGeneration++;
    lastScrollTop = current()?.scrollTop ?? lastScrollTop;
  }

  function scrollToBottom(): void {
    following.value = true;
    const el = current();
    if (!el) return;
    el.scrollTop = el.scrollHeight;
    lastScrollTop = el.scrollTop;
  }

  return { following, onScroll, onContentGrow, onUserIntent, scrollToBottom };
}
