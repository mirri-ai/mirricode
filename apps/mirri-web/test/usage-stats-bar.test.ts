// test/usage-stats-bar.test.ts
//
// UsageStatsBar group construction (usage-visibility Task 7). The repo has no
// @vue/test-utils, so the component's group construction is a named pure
// export (`buildStatsGroups`) and tested directly:
// - group order is [turn count, cache hit, after-cold-start hit, cache read,
//   uncached input, cache write, output] (ADR-004 priced buckets), and each
//   group carries the drop-priority key that matches its text (so a
//   key↔text mismatch cannot slip through),
// - an all-failed session (no billed input, no output) renders only the turn
//   count — or nothing at all when no turn completed (US1.2),
// - the hit group needs billed input; zero billed input with output keeps
//   the output group and drops the hit group,
// - a full hit renders exactly 100, a partial one never does (SDR-001 d),
// - token counts use exact grouping (no 1.2k rounding),
// - splitInputBuckets segments the input buckets per priced tier,
// - fitSegments drops low-value groups first on narrow widths (ADR-004),
// - measureGroupWidths keys widths by data-group so a dropped group keeps
//   its last measured width instead of polluting the next fitting decision.
//
// The ResizeObserver wiring itself (onMounted/observer/measure in
// UsageStatsBar.vue) is NOT covered here: jsdom has no ResizeObserver, so
// the width wiring is exercised only in a real browser. The key→width
// mapping it relies on is extracted as the pure measureGroupWidths and
// tested below.

import { beforeAll, describe, expect, it } from 'vitest';
import { buildStatsGroups } from '../src/components/chat/UsageStatsBar.vue';
import {
  billedInputTokens,
  fitSegments,
  measureGroupWidths,
  splitInputBuckets,
  type GroupWidths,
  type SizedSegment,
} from '../src/lib/usageStats';
import type { AppSessionUsage } from '../src/api/types';
import { i18n } from '../src/i18n';

function usage(partial: Partial<AppSessionUsage> = {}): AppSessionUsage {
  return {
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    cacheCreationTokens: 0,
    totalCostUsd: 0,
    contextTokens: 0,
    contextLimit: 0,
    turnCount: 0,
    ...partial,
  };
}

/** Assert both the keys and the texts of the built groups, so a key↔text
 *  mismatch fails the assertion even when the texts alone look right. */
function expectGroups(u: AppSessionUsage, keys: string[], texts: string[]): void {
  const groups = buildStatsGroups(u);
  expect(groups.map((g) => g.key)).toEqual(keys);
  expect(groups.map((g) => g.text)).toEqual(texts);
}

beforeAll(() => {
  // The group strings are locale-formatted; pin English so the expectations
  // below are deterministic regardless of the runner's navigator language.
  i18n.global.locale.value = 'en';
});

describe('buildStatsGroups', () => {
  it('emits the cache-hit and priced-bucket groups in order', () => {
    expectGroups(
      usage({ inputTokens: 100, cacheReadTokens: 400, outputTokens: 5 }),
      ['cacheHit', 'cacheRead', 'input', 'output'],
      ['Cache hit 80.00%', 'Cache read 400', 'Uncached input 100', 'Out 5'],
    );
  });

  it('renders no groups for an all-failed session (no billed input, no output)', () => {
    expect(buildStatsGroups(usage())).toEqual([]);
  });

  it('renders only the turn group for an all-failed session that completed turns', () => {
    expectGroups(usage({ turnCount: 3 }), ['turnCount'], ['3 turns']);
  });

  it('keeps the output group but drops the hit and input segments when there is output but no billed input', () => {
    expectGroups(usage({ outputTokens: 7 }), ['output'], ['Out 7']);
  });

  it('keeps the turn and output groups when there is output but no billed input and turns completed', () => {
    expectGroups(
      usage({ outputTokens: 7, turnCount: 2 }),
      ['turnCount', 'output'],
      ['2 turns', 'Out 7'],
    );
  });

  it('shows the hit group (even 0%) whenever there is billed input', () => {
    expectGroups(
      usage({ inputTokens: 100, outputTokens: 3 }),
      ['cacheHit', 'cacheRead', 'input', 'output'],
      ['Cache hit 0.00%', 'Cache read 0', 'Uncached input 100', 'Out 3'],
    );
  });

  it('formats the real-data example as a 93.92% cache hit with exact token grouping', () => {
    expectGroups(
      usage({ inputTokens: 72543, cacheReadTokens: 1120576, cacheCreationTokens: 0, outputTokens: 3204 }),
      ['cacheHit', 'cacheRead', 'input', 'output'],
      ['Cache hit 93.92%', 'Cache read 1,120,576', 'Uncached input 72,543', 'Out 3,204'],
    );
  });

  it('renders a full cache hit as exactly 100', () => {
    expectGroups(
      usage({ cacheReadTokens: 512 }),
      ['cacheHit', 'cacheRead', 'input'],
      ['Cache hit 100%', 'Cache read 512', 'Uncached input 0'],
    );
  });

  it('never renders a partial hit as 100 (anti-fake-full-hit)', () => {
    const groups = buildStatsGroups(
      usage({ inputTokens: 1, cacheReadTokens: 999999, cacheCreationTokens: 0 }),
    );
    expect(groups[0].text).not.toBe('Cache hit 100%');
    expect(groups[0].text).toBe('Cache hit 99.9999%');
  });

  it('counts the cache-write bucket in the hit denominator (SDR-001 denominator)', () => {
    const groups = buildStatsGroups(
      usage({ inputTokens: 10, cacheReadTokens: 20, cacheCreationTokens: 70 }),
    );
    expect(groups[0].text).toBe('Cache hit 20.00%');
  });

  it('omits the cache-write group when the session never wrote cache', () => {
    const groups = buildStatsGroups(usage({ inputTokens: 100, cacheReadTokens: 400, outputTokens: 5 }));
    expect(groups.some((g) => g.key === 'cacheWrite')).toBe(false);
  });

  it('shows a 0% hit, a zero cache read, and the write for a cold-start session', () => {
    expectGroups(
      usage({ inputTokens: 100, cacheCreationTokens: 50, outputTokens: 5 }),
      ['cacheHit', 'cacheRead', 'input', 'cacheWrite', 'output'],
      ['Cache hit 0.00%', 'Cache read 0', 'Uncached input 100', 'Cache write 50', 'Out 5'],
    );
  });

  it('formats in the zh locale when selected', () => {
    const prev = i18n.global.locale.value;
    i18n.global.locale.value = 'zh';
    try {
      expectGroups(
        usage({ inputTokens: 100, cacheReadTokens: 400, outputTokens: 5 }),
        ['cacheHit', 'cacheRead', 'input', 'output'],
        ['缓存命中 80.00%', '缓存读取 400', '未缓存输入 100', '输出 5'],
      );
    } finally {
      i18n.global.locale.value = prev;
    }
  });

  it('appends the after-cold-start group when the first call is known', () => {
    expectGroups(
      usage({
        inputTokens: 100,
        cacheReadTokens: 400,
        outputTokens: 5,
        firstCall: { inputTokens: 90, outputTokens: 0, cacheReadTokens: 10, cacheCreationTokens: 0 },
      }),
      ['cacheHit', 'afterColdStart', 'cacheRead', 'input', 'output'],
      ['Cache hit 80.00%', 'After cold start 97.50%', 'Cache read 400', 'Uncached input 100', 'Out 5'],
    );
  });

  it('omits the after-cold-start group when the first call is unknown', () => {
    const groups = buildStatsGroups(usage({ inputTokens: 100, cacheReadTokens: 400 }));
    expect(groups.map((g) => g.key)).toEqual(['cacheHit', 'cacheRead', 'input']);
  });

  it('omits the after-cold-start group when the session made a single call', () => {
    const groups = buildStatsGroups(
      usage({
        inputTokens: 100,
        cacheReadTokens: 400,
        firstCall: { inputTokens: 100, outputTokens: 0, cacheReadTokens: 400, cacheCreationTokens: 0 },
      }),
    );
    expect(groups.map((g) => g.key)).toEqual(['cacheHit', 'cacheRead', 'input']);
  });

  it('emits the turn group first when the session has completed turns', () => {
    expectGroups(
      usage({ turnCount: 12, inputTokens: 100, outputTokens: 5 }),
      ['turnCount', 'cacheHit', 'cacheRead', 'input', 'output'],
      ['12 turns', 'Cache hit 0.00%', 'Cache read 0', 'Uncached input 100', 'Out 5'],
    );
  });
});

describe('splitInputBuckets', () => {
  it('should return only the cache-read and uncached-input segments when cache write is zero', () => {
    expect(splitInputBuckets({ inputTokens: 100, cacheReadTokens: 400, cacheCreationTokens: 0 })).toEqual([
      { key: 'cacheRead', tokens: 400 },
      { key: 'input', tokens: 100 },
    ]);
  });

  it('should return all three segments when cache write is positive', () => {
    expect(splitInputBuckets({ inputTokens: 10, cacheReadTokens: 20, cacheCreationTokens: 70 })).toEqual([
      { key: 'cacheRead', tokens: 20 },
      { key: 'input', tokens: 10 },
      { key: 'cacheWrite', tokens: 70 },
    ]);
  });

  it('should still emit zero-valued cache-read and uncached-input segments when all buckets are zero', () => {
    expect(splitInputBuckets({ inputTokens: 0, cacheReadTokens: 0, cacheCreationTokens: 0 })).toEqual([
      { key: 'cacheRead', tokens: 0 },
      { key: 'input', tokens: 0 },
    ]);
  });

  it('should keep the segment sum equal to billedInputTokens even when the write segment is omitted', () => {
    const cases = [
      { inputTokens: 0, cacheReadTokens: 0, cacheCreationTokens: 0 },
      { inputTokens: 10, cacheReadTokens: 20, cacheCreationTokens: 70 },
      { inputTokens: 100, cacheReadTokens: 400, cacheCreationTokens: 0 },
    ];
    for (const u of cases) {
      const sum = splitInputBuckets(u).reduce((acc, s) => acc + s.tokens, 0);
      expect(sum).toBe(billedInputTokens(u));
    }
  });
});

describe('fitSegments', () => {
  /** Fixture: the full 7-group bar with realistic widths. */
  function fullBar(): SizedSegment[] {
    return [
      { key: 'turnCount', text: '12 turns', width: 60 },
      { key: 'cacheHit', text: 'Cache hit 80.00%', width: 120 },
      { key: 'afterColdStart', text: 'After cold start 97.50%', width: 180 },
      { key: 'cacheRead', text: 'Cache read 400', width: 130 },
      { key: 'input', text: 'Uncached input 100', width: 150 },
      { key: 'cacheWrite', text: 'Cache write 50', width: 120 },
      { key: 'output', text: 'Out 5', width: 45 },
    ];
  }

  it('should keep only the cache-hit and input groups when the width barely fits two', () => {
    const kept = fitSegments(fullBar(), 280);
    expect(kept.map((s) => s.key)).toEqual(['cacheHit', 'input']);
  });

  it('should keep every group when the width is sufficient', () => {
    expect(fitSegments(fullBar(), 1000)).toHaveLength(7);
  });

  it('should drop the turn-count group first when only one group must go', () => {
    // Total width is 805; at 800 the lowest-value group (turn count, 60) is dropped.
    const kept = fitSegments(fullBar(), 800);
    expect(kept.map((s) => s.key)).not.toContain('turnCount');
    expect(kept).toHaveLength(6);
  });

  it('should return an empty array for an empty segment list', () => {
    expect(fitSegments([], 100)).toEqual([]);
  });
});

describe('measureGroupWidths', () => {
  /** Element stub matching the shape measureGroupWidths reads: the
   *  data-group key and the measured clientWidth. */
  function el(key: string | null, width: number): { dataset: { group?: string }; clientWidth: number } {
    return {
      dataset: key === null ? {} : { group: key },
      clientWidth: width,
    };
  }

  it('keys the measured widths by their data-group value', () => {
    const widths = measureGroupWidths([el('turnCount', 60), el('cacheHit', 120)], {});
    expect(widths).toEqual({ turnCount: 60, cacheHit: 120 });
  });

  it('keeps the last measured width for groups that are no longer rendered', () => {
    // Steady state: all five groups measured. Then the fitting dropped the
    // turn-count and output groups — a re-measure must NOT overwrite their
    // widths (e.g. with 0 or a separator's width), or the next fitting
    // decision would falsely believe everything fits again.
    const measured: GroupWidths = measureGroupWidths(
      [el('turnCount', 60), el('cacheHit', 120), el('cacheRead', 130), el('input', 150), el('output', 45)],
      {},
    );
    const afterDropped = measureGroupWidths([el('cacheHit', 120), el('cacheRead', 130), el('input', 150)], measured);
    expect(afterDropped).toEqual(measured);
  });

  it('updates the width of a still-rendered group while keeping dropped ones', () => {
    const measured: GroupWidths = measureGroupWidths([el('cacheHit', 120), el('input', 150)], {});
    const updated = measureGroupWidths([el('cacheHit', 135), el('input', 150)], measured);
    expect(updated).toEqual({ cacheHit: 135, input: 150 });
  });

  it('ignores elements without a known group key (e.g. separators)', () => {
    const widths = measureGroupWidths([el(null, 20), el('unknown', 30), el('cacheHit', 120)], {});
    expect(widths).toEqual({ cacheHit: 120 });
  });

  it('does not mutate the previous record', () => {
    const previous: GroupWidths = { cacheHit: 120 };
    measureGroupWidths([el('input', 150)], previous);
    expect(previous).toEqual({ cacheHit: 120 });
  });

  it('feeds the full-measure-then-drop-then-remeasure cycle without polluting the fitting decision', () => {
    // End-to-end over the pure pieces, mirroring the component's lifecycle:
    // measure the full bar, let fitSegments drop groups on a narrow width,
    // re-measure only what is still rendered, then run the fitting again —
    // the kept groups must be identical to the first decision (a dropped
    // group's stale width must not decay to 0 and let everything back in).
    const full: SizedSegment[] = [
      { key: 'turnCount', text: '12 turns', width: 60 },
      { key: 'cacheHit', text: 'Cache hit 80.00%', width: 120 },
      { key: 'cacheRead', text: 'Cache read 400', width: 130 },
      { key: 'input', text: 'Uncached input 100', width: 150 },
      { key: 'output', text: 'Out 5', width: 45 },
    ];
    const measured = measureGroupWidths(
      full.map((s) => el(s.key, s.width)),
      {},
    );
    const keptFirst = fitSegments(full, 460); // drops turnCount (60)
    expect(keptFirst.map((s) => s.key)).toEqual(['cacheHit', 'cacheRead', 'input', 'output']);
    // The DOM now renders only the kept groups; output is dropped here too
    // to prove a double-drop also keeps the earlier width intact.
    const remeasured = measureGroupWidths(
      keptFirst.filter((s) => s.key !== 'output').map((s) => el(s.key, s.width)),
      measured,
    );
    const secondAttempt = full.map((s) => ({ ...s, width: remeasured[s.key] ?? 0 }));
    expect(fitSegments(secondAttempt, 460).map((s) => s.key)).toEqual(keptFirst.map((s) => s.key));
  });
});
