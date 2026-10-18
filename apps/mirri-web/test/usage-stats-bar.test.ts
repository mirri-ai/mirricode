// test/usage-stats-bar.test.ts
//
// UsageStatsBar group construction (usage-visibility Task 7). The repo has no
// @vue/test-utils, so the component's group construction is a named pure
// export (`buildStatsGroups`) and tested directly:
// - group order is [cache hit, in/out tokens],
// - an all-failed session (no billed input, no output) renders NO groups —
//   in particular no fake "0%" hit line (US1.2),
// - the hit group needs billed input; zero billed input with output keeps
//   the token group and drops the hit group,
// - a full hit renders exactly 100, a partial one never does (SDR-001 d),
// - token counts use exact grouping (no 1.2k rounding).

import { beforeAll, describe, expect, it } from 'vitest';
import { buildStatsGroups } from '../src/components/chat/UsageStatsBar.vue';
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

beforeAll(() => {
  // The group strings are locale-formatted; pin English so the expectations
  // below are deterministic regardless of the runner's navigator language.
  i18n.global.locale.value = 'en';
});

describe('buildStatsGroups', () => {
  it('emits the cache-hit group before the token group', () => {
    const groups = buildStatsGroups(usage({ inputTokens: 100, cacheReadTokens: 400, outputTokens: 5 }));
    expect(groups).toHaveLength(2);
    expect(groups[0]).toBe('Cache hit 80.00%');
    expect(groups[1]).toBe('In 500 · Out 5');
  });

  it('renders no groups for an all-failed session (no billed input, no output)', () => {
    expect(buildStatsGroups(usage())).toEqual([]);
  });

  it('keeps the token group but drops the hit group when there is output but no billed input', () => {
    const groups = buildStatsGroups(usage({ outputTokens: 7 }));
    expect(groups).toEqual(['In 0 · Out 7']);
  });

  it('shows the hit group (even 0%) whenever there is billed input', () => {
    const groups = buildStatsGroups(usage({ inputTokens: 100, outputTokens: 3 }));
    expect(groups).toEqual(['Cache hit 0.00%', 'In 100 · Out 3']);
  });

  it('formats the real-data example as a 93.92% cache hit with exact token grouping', () => {
    const groups = buildStatsGroups(
      usage({ inputTokens: 72543, cacheReadTokens: 1120576, cacheCreationTokens: 0, outputTokens: 3204 }),
    );
    expect(groups[0]).toBe('Cache hit 93.92%');
    expect(groups[1]).toBe('In 1,193,119 · Out 3,204');
  });

  it('renders a full cache hit as exactly 100', () => {
    const groups = buildStatsGroups(usage({ cacheReadTokens: 512 }));
    expect(groups[0]).toBe('Cache hit 100%');
    expect(groups[1]).toBe('In 512 · Out 0');
  });

  it('never renders a partial hit as 100 (anti-fake-full-hit)', () => {
    const groups = buildStatsGroups(
      usage({ inputTokens: 1, cacheReadTokens: 999999, cacheCreationTokens: 0 }),
    );
    expect(groups[0]).not.toBe('Cache hit 100%');
    expect(groups[0]).toBe('Cache hit 99.9999%');
  });

  it('counts the cache-write bucket in the billed input total (SDR-001 denominator)', () => {
    const groups = buildStatsGroups(
      usage({ inputTokens: 10, cacheReadTokens: 20, cacheCreationTokens: 70 }),
    );
    expect(groups[1]).toBe('In 100 · Out 0');
  });

  it('formats in the zh locale when selected', () => {
    const prev = i18n.global.locale.value;
    i18n.global.locale.value = 'zh';
    try {
      const groups = buildStatsGroups(usage({ inputTokens: 100, cacheReadTokens: 400, outputTokens: 5 }));
      expect(groups).toEqual(['缓存命中 80.00%', '输入 500 · 输出 5']);
    } finally {
      i18n.global.locale.value = prev;
    }
  });

  it('appends the after-cold-start group when the first call is known', () => {
    const groups = buildStatsGroups(
      usage({
        inputTokens: 100,
        cacheReadTokens: 400,
        outputTokens: 5,
        firstCall: { inputTokens: 90, outputTokens: 0, cacheReadTokens: 10, cacheCreationTokens: 0 },
      }),
    );
    expect(groups[0]).toBe('Cache hit 80.00%');
    // (400-10) / (100+400-90-10) = 390/400 = 97.50%
    expect(groups[1]).toBe('After cold start 97.50%');
    expect(groups[2]).toBe('In 500 · Out 5');
  });

  it('omits the after-cold-start group when the first call is unknown', () => {
    const groups = buildStatsGroups(usage({ inputTokens: 100, cacheReadTokens: 400 }));
    expect(groups).toHaveLength(2);
  });

  it('omits the after-cold-start group when the session made a single call', () => {
    const groups = buildStatsGroups(
      usage({
        inputTokens: 100,
        cacheReadTokens: 400,
        firstCall: { inputTokens: 100, outputTokens: 0, cacheReadTokens: 400, cacheCreationTokens: 0 },
      }),
    );
    expect(groups).toHaveLength(2);
  });
});
