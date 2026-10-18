// apps/mirri-web/src/lib/scrollFollow.test.ts
import { describe, expect, it } from 'vitest';
import { nextTick } from 'vue';
import {
  BOTTOM_FOLLOW_THRESHOLD_PX,
  createScrollFollow,
  isNearBottom,
} from './scrollFollow';

function el(distanceFromBottom: number): { scrollHeight: number; scrollTop: number; clientHeight: number } {
  const clientHeight = 400;
  const scrollHeight = 1000;
  return { scrollHeight, clientHeight, scrollTop: scrollHeight - clientHeight - distanceFromBottom };
}

describe('isNearBottom', () => {
  it('should follow the bottom when the scrollbar is at the very bottom', () => {
    expect(isNearBottom(el(0))).toBe(true);
  });

  it(`should follow the bottom when the scrollbar is within ${BOTTOM_FOLLOW_THRESHOLD_PX}px of the bottom`, () => {
    expect(isNearBottom(el(BOTTOM_FOLLOW_THRESHOLD_PX - 1))).toBe(true);
  });

  it(`should stop following when the scrollbar is ${BOTTOM_FOLLOW_THRESHOLD_PX}px or more from the bottom`, () => {
    expect(isNearBottom(el(BOTTOM_FOLLOW_THRESHOLD_PX))).toBe(false);
    expect(isNearBottom(el(BOTTOM_FOLLOW_THRESHOLD_PX + 500))).toBe(false);
  });

  it('should follow the bottom when the content does not overflow (nothing to scroll)', () => {
    const short = { scrollHeight: 300, scrollTop: 0, clientHeight: 400 };
    expect(isNearBottom(short)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// createScrollFollow — direction-based follow state machine (the pattern the
// main conversation pane uses). The pure-threshold variant ("near bottom ⇒
// keep writing scrollTop = scrollHeight") loses to streaming: every follow
// write resets the distance to 0, so a trackpad flick up of a few px never
// accumulates past the threshold before the next delta snaps it back down.

/** Fake scroll container driven by the tests. Like a real element it clamps
 *  scrollTop to [0, scrollHeight - clientHeight] on assignment. */
function container(opts?: { scrollHeight?: number; clientHeight?: number }) {
  const c = {
    scrollHeight: opts?.scrollHeight ?? 1000,
    clientHeight: opts?.clientHeight ?? 400,
    _top: 0,
    get scrollTop(): number {
      return c._top;
    },
    set scrollTop(v: number) {
      c._top = Math.max(0, Math.min(v, c.scrollHeight - c.clientHeight));
    },
  };
  return c;
}

/** The container grows by one streaming delta (120px). */
function contentGrew(el: ReturnType<typeof container>): void {
  el.scrollHeight += 120;
}

/** Let the composable's queued nextTick follow-write run (or get skipped). */
const flush = (): Promise<void> => nextTick();

describe('createScrollFollow', () => {
  it('should follow the bottom while new content streams in', async () => {
    const el = container();
    const f = createScrollFollow(() => el);
    contentGrew(el);
    await f.onContentGrow();
    expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight);
    expect(f.following.value).toBe(true);
  });

  it('should stop following when the user scrolls up, even a few pixels at a time', async () => {
    const el = container();
    const f = createScrollFollow(() => el);
    contentGrew(el);
    await f.onContentGrow();

    // Trackpad flick: only 10px up — far below any threshold, but a real
    // upward direction. The next streaming delta must NOT snap it back down.
    el.scrollTop -= 10;
    f.onScroll();
    expect(f.following.value).toBe(false);

    const parked = el.scrollTop;
    contentGrew(el);
    await f.onContentGrow();
    expect(el.scrollTop).toBe(parked);
  });

  it('should treat sub-pixel drift as noise and keep following', async () => {
    const el = container();
    const f = createScrollFollow(() => el);
    contentGrew(el);
    await f.onContentGrow();

    el.scrollTop -= 1;
    f.onScroll();
    expect(f.following.value).toBe(true);
    contentGrew(el);
    await f.onContentGrow();
    expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight);
  });

  it('should stop following on upward user intent before the scroll event fires', async () => {
    const el = container();
    const f = createScrollFollow(() => el);
    contentGrew(el);
    await f.onContentGrow();

    // An upward wheel tick arrives before the browser dispatches `scroll` —
    // the queued follow write for an already-arrived delta must be cancelled.
    const pending = f.onContentGrow();
    f.onUserIntent('up');
    await pending;
    expect(f.following.value).toBe(false);
    expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight);

    await flush();
    expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight);
  });

  it('should not stop following on downward user intent', async () => {
    const el = container();
    const f = createScrollFollow(() => el);
    contentGrew(el);
    await f.onContentGrow();

    f.onUserIntent('down');
    expect(f.following.value).toBe(true);
    contentGrew(el);
    await f.onContentGrow();
    expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight);
  });

  it('should not stop following on upward intent when the content does not overflow', () => {
    // Nothing to scroll — there is no "above" to read, so the intent is moot.
    const el = container({ scrollHeight: 300 });
    const f = createScrollFollow(() => el);
    f.onUserIntent('up');
    f.onScroll();
    expect(f.following.value).toBe(true);
  });

  it('should resume following when the user scrolls back down into the bottom zone', async () => {
    const el = container();
    const f = createScrollFollow(() => el);
    contentGrew(el);
    await f.onContentGrow();

    el.scrollTop -= 300; // well out of the follow zone
    f.onScroll();
    expect(f.following.value).toBe(false);

    // Scrolling down while still outside the zone does not resume…
    el.scrollTop += 200;
    f.onScroll();
    expect(f.following.value).toBe(false);
    // …crossing into the zone downward re-enables the follow.
    el.scrollTop += 90;
    f.onScroll();
    expect(f.following.value).toBe(true);
    contentGrew(el);
    await f.onContentGrow();
    expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight);
  });

  it('should leave the position alone while not following, even as content keeps streaming', async () => {
    const el = container();
    const f = createScrollFollow(() => el);
    contentGrew(el);
    await f.onContentGrow();

    el.scrollTop -= 300;
    f.onScroll();
    const parked = el.scrollTop;
    contentGrew(el);
    contentGrew(el);
    await f.onContentGrow();
    // 240px of new content grew below; the viewport stayed put.
    expect(el.scrollTop).toBe(parked);
  });

  it('should do nothing when the container is missing', async () => {
    let el: ReturnType<typeof container> | null = null;
    const f = createScrollFollow(() => el);
    await f.onContentGrow();
    f.onScroll();
    f.onUserIntent('up');
    expect(f.following.value).toBe(true); // no container → nothing to stop

    el = container();
    contentGrew(el);
    f.scrollToBottom();
    expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight);
  });

  it('should re-enable the follow on an explicit scrollToBottom', async () => {
    const el = container();
    const f = createScrollFollow(() => el);
    contentGrew(el);
    await f.onContentGrow();

    el.scrollTop -= 300;
    f.onScroll();
    expect(f.following.value).toBe(false);

    f.scrollToBottom();
    expect(f.following.value).toBe(true);
    expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight);

    contentGrew(el);
    await f.onContentGrow();
    expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight);
  });
});
